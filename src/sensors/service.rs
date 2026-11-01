// The sensor service: a Windows service that reads the CPU sensors through PawnIO and sends
// the values over a named pipe. The app can then show them with no admin prompt, and it
// can't reach the driver itself. Non-admin programs may only read from the pipe, so all
// they get is sensor values.
//
// It only touches the driver while a client is connected.

use std::fs::File;
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::{w, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerSessionId,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};

use super::cpu::Cpu;

const LOGS_FOLDER: &str = "logs";
const PIPE_PATH: &str = r"\\.\pipe\MonitorSensors";
/// SYSTEM and admins get full access. Signed-in users get read access (FILE_GENERIC_READ)
/// and nothing else, so they can't send anything or create a pipe of the same name.
const PIPE_SECURITY: windows::core::PCWSTR =
    w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x120089;;;AU)");
const MAX_CLIENTS: u32 = 8;
const PIPE_BUFFER_BYTES: u32 = 8192;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Change this when the messages change. An app that sees another number asks to update
/// the service, because the installed service doesn't update along with the app.
const PROTOCOL: u32 = 1;
const END_MARKER: &str = "end=1";

const SAMPLE_PERIOD: Duration = Duration::from_millis(800);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(5);
const RECONNECT_PERIOD: Duration = Duration::from_secs(1);
/// How long the service can go quiet before it's considered gone. Updates can stall for
/// several seconds with every core busy, which shouldn't look like a failure.
const SILENCE_LIMIT_SECS: u64 = 10;

/// Where the service keeps its logs, next to its exe, in a folder only admins can write to.
pub fn logs_folder() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join(LOGS_FOLDER))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One update from the service.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Reading {
    pub ts: u64,
    pub temp: Option<f32>,
    pub power: Option<f32>,
    pub cores: Vec<f32>,
    /// Why there are no values.
    pub error: Option<String>,
}

fn format_message(r: &Reading) -> String {
    let cores = r
        .cores
        .iter()
        .map(|c| format!("{c:.0}"))
        .collect::<Vec<_>>()
        .join(",");
    let error = r.error.as_deref().unwrap_or("").replace(['\r', '\n'], " ");
    format!(
        "protocol={PROTOCOL}\nts={}\ntemp={}\npower={}\ncores={cores}\nerror={error}\n{END_MARKER}\n",
        r.ts,
        r.temp.map(|v| format!("{v:.2}")).unwrap_or_default(),
        r.power.map(|v| format!("{v:.1}")).unwrap_or_default(),
    )
}

#[derive(Debug, PartialEq)]
enum Message {
    Reading(Reading),
    OtherProtocol,
}

fn parse_message(text: &str) -> Option<Message> {
    let mut protocol = None;
    let mut r = Reading::default();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("protocol=") {
            protocol = v.trim().parse::<u32>().ok();
        } else if let Some(v) = line.strip_prefix("ts=") {
            r.ts = v.trim().parse().ok()?;
        } else if let Some(v) = line.strip_prefix("temp=") {
            r.temp = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("power=") {
            r.power = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("cores=") {
            r.cores = v.trim().split(',').filter_map(|x| x.parse().ok()).collect();
        } else if let Some(v) = line.strip_prefix("error=") {
            r.error = Some(v.trim().to_string()).filter(|e| !e.is_empty());
        }
    }
    match protocol? {
        PROTOCOL => Some(Message::Reading(r)),
        _ => Some(Message::OtherProtocol),
    }
}

/// Takes the first whole message off the front of `buffer`.
fn take_message(buffer: &mut String) -> Option<String> {
    let marker = format!("{END_MARKER}\n");
    let end = buffer.find(&marker)? + marker.len();
    Some(buffer.drain(..end).collect())
}

fn reading_is_current(ts: u64, now: u64) -> bool {
    now.saturating_sub(ts) <= SILENCE_LIMIT_SECS
}

// ---- The app's side -------------------------------------------------------------------

#[derive(Default)]
struct ClientState {
    last: Option<Reading>,
    other_protocol: bool,
}

/// Keeps connecting to the service in the background and holds its latest reading.
pub struct Client {
    state: Arc<Mutex<ClientState>>,
}

impl Client {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(ClientState::default()));
        let thread_state = state.clone();
        std::thread::spawn(move || loop {
            if let Some(pipe) = connect() {
                read_messages(pipe, &thread_state);
            }
            std::thread::sleep(RECONNECT_PERIOD);
        });
        Self { state }
    }

    /// The latest reading, unless it's too old to count.
    pub fn reading(&self) -> Option<Reading> {
        let state = self.state.lock().unwrap();
        state
            .last
            .clone()
            .filter(|r| reading_is_current(r.ts, now_secs()))
    }

    /// Whether the running service is from a version of the app this one can't talk to.
    pub fn other_protocol(&self) -> bool {
        self.state.lock().unwrap().other_protocol
    }
}

/// Any program could create a pipe of this name while the service isn't running, so this
/// also checks that the other end is in session 0, where only services run. It also asks
/// Windows not to let the other end act as this user.
fn connect() -> Option<File> {
    const SECURITY_IDENTIFICATION: u32 = 1 << 16;
    let pipe = std::fs::OpenOptions::new()
        .read(true)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(PIPE_PATH)
        .ok()?;
    let mut session = u32::MAX;
    let checked = unsafe {
        GetNamedPipeServerSessionId(HANDLE(pipe.as_raw_handle()), &mut session)
    };
    if checked.is_err() || session != 0 {
        log::warn!("ignoring a sensor pipe that isn't from a service, session {session}");
        return None;
    }
    Some(pipe)
}

fn read_messages(mut pipe: File, state: &Mutex<ClientState>) {
    let mut buffer = String::new();
    let mut chunk = [0u8; 4096];
    while let Ok(read) = pipe.read(&mut chunk) {
        if read == 0 {
            return;
        }
        buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
        if buffer.len() > MAX_MESSAGE_BYTES {
            log::warn!("the sensor service sent too much, reconnecting");
            return;
        }
        while let Some(text) = take_message(&mut buffer) {
            let mut state = state.lock().unwrap();
            match parse_message(&text) {
                Some(Message::Reading(reading)) => {
                    state.other_protocol = false;
                    state.last = Some(reading);
                }
                Some(Message::OtherProtocol) => {
                    state.other_protocol = true;
                    state.last = None;
                }
                None => {}
            }
        }
    }
}

// ---- The service's side ---------------------------------------------------------------

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);

fn report_status(state: SERVICE_STATUS_CURRENT_STATE) {
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
    } else {
        0
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        ..Default::default()
    };
    let handle = SERVICE_STATUS_HANDLE(STATUS_HANDLE.load(Ordering::SeqCst) as *mut _);
    if let Err(e) = unsafe { SetServiceStatus(handle, &status) } {
        log::warn!("can't report the service status: {}", e.message());
    }
}

unsafe extern "system" fn control_handler(control: u32) {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        log::info!("sensor service stopping");
        STOP.store(true, Ordering::SeqCst);
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let Ok(handle) = RegisterServiceCtrlHandlerW(w!("MonitorSensors"), Some(control_handler))
    else {
        log::error!("can't register the service handler");
        return;
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::SeqCst);
    report_status(SERVICE_RUNNING);
    log::info!("sensor service {} running", env!("CARGO_PKG_VERSION"));

    let hub = Arc::new(Hub::default());
    let sampler_hub = hub.clone();
    std::thread::spawn(move || sample_while_clients_connected(&sampler_hub));
    std::thread::spawn(move || serve_pipe(&hub));

    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }
    // The other threads end with the process, which also closes the driver.
    report_status(SERVICE_STOPPED);
}

/// Entry point for `--service`. Returns once the service has stopped.
pub fn run_service() {
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(w!("MonitorSensors").as_ptr() as *mut _),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    if let Err(e) = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        log::error!("not started as a service: {}", e.message());
    }
}

/// What connects the sampling thread to the threads serving clients.
#[derive(Default)]
struct Hub {
    clients: AtomicU32,
    /// The latest message, with a number that goes up with each one.
    latest: Mutex<(u64, String)>,
}

impl Hub {
    fn publish(&self, reading: &Reading) {
        let mut latest = self.latest.lock().unwrap();
        latest.0 += 1;
        latest.1 = format_message(reading);
    }
}

fn sample_while_clients_connected(hub: &Hub) {
    // Readings only matter if they keep coming, including with every core busy.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let mut cpu = None;
    let mut last_error = None;
    loop {
        if hub.clients.load(Ordering::SeqCst) == 0 {
            if cpu.take().is_some() {
                log::info!("no clients, driver closed");
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        if cpu.is_none() {
            match Cpu::init() {
                Ok(opened) => {
                    log::info!("driver opened");
                    cpu = Some(opened);
                    last_error = None;
                }
                Err(e) => {
                    if last_error.as_ref() != Some(&e) {
                        log::error!("can't read the sensors: {e}");
                    }
                    hub.publish(&Reading {
                        ts: now_secs(),
                        error: Some(e.clone()),
                        ..Default::default()
                    });
                    last_error = Some(e);
                    std::thread::sleep(RETRY_AFTER_ERROR);
                    continue;
                }
            }
        }
        let cpu: &mut Cpu = cpu.as_mut().unwrap();
        let temp = cpu.temp();
        let cores = cpu.core_clocks();
        let power = cpu.power();
        hub.publish(&Reading {
            ts: now_secs(),
            temp,
            power,
            cores,
            error: None,
        });
        std::thread::sleep(SAMPLE_PERIOD);
    }
}

/// Keeps a pipe instance waiting for the next client, and serves each on its own thread.
fn serve_pipe(hub: &Arc<Hub>) {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    if let Err(e) = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PIPE_SECURITY,
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    } {
        log::error!("can't set up the pipe's permissions: {}", e.message());
        return;
    }
    // The descriptor lives as long as the process, so it isn't freed.
    let security = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let name = windows::core::HSTRING::from(PIPE_PATH);

    let mut first = true;
    loop {
        // The first one fails if another program already took the name.
        let flags: FILE_FLAGS_AND_ATTRIBUTES = if first {
            PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            PIPE_ACCESS_OUTBOUND
        };
        let pipe = unsafe {
            CreateNamedPipeW(
                &name,
                flags,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_CLIENTS,
                PIPE_BUFFER_BYTES,
                0,
                0,
                Some(&security),
            )
        };
        if pipe == INVALID_HANDLE_VALUE || pipe.is_invalid() {
            log::warn!(
                "can't create a pipe instance: {}",
                windows::core::Error::from_win32().message()
            );
            std::thread::sleep(RETRY_AFTER_ERROR);
            continue;
        }
        first = false;

        let connected = match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if !connected {
            unsafe {
                let _ = CloseHandle(pipe);
            }
            continue;
        }
        hub.clients.fetch_add(1, Ordering::SeqCst);
        let hub = hub.clone();
        let raw = pipe.0 as isize;
        std::thread::spawn(move || {
            serve_client(HANDLE(raw as *mut _), &hub);
            hub.clients.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn serve_client(pipe: HANDLE, hub: &Hub) {
    let mut sent = 0;
    loop {
        let message = {
            let latest = hub.latest.lock().unwrap();
            (latest.0 != sent).then(|| (latest.0, latest.1.clone()))
        };
        if let Some((number, text)) = message {
            let mut written = 0u32;
            if unsafe { WriteFile(pipe, Some(text.as_bytes()), Some(&mut written), None) }.is_err()
            {
                break;
            }
            sent = number;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        let _ = DisconnectNamedPipe(pipe);
        let _ = CloseHandle(pipe);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading_at(ts: u64) -> Reading {
        Reading {
            ts,
            temp: Some(47.5),
            power: Some(12.5),
            cores: vec![4500.0, 4400.0],
            error: None,
        }
    }

    #[test]
    fn messages_round_trip() {
        let r = reading_at(123);
        assert_eq!(parse_message(&format_message(&r)), Some(Message::Reading(r)));
    }

    #[test]
    fn missing_values_stay_missing() {
        let r = Reading {
            ts: 5,
            ..Default::default()
        };
        assert_eq!(parse_message(&format_message(&r)), Some(Message::Reading(r)));
    }

    #[test]
    fn errors_come_through_on_one_line() {
        let r = Reading {
            ts: 5,
            error: Some("no\r\ndriver=here".to_string()),
            ..Default::default()
        };
        let Some(Message::Reading(parsed)) = parse_message(&format_message(&r)) else {
            panic!("not a reading");
        };
        assert_eq!(parsed.error.as_deref(), Some("no  driver=here"));
    }

    #[test]
    fn a_service_of_another_version_is_recognized() {
        assert_eq!(
            parse_message("protocol=999\nts=1\nend=1\n"),
            Some(Message::OtherProtocol)
        );
        assert_eq!(parse_message("ts=1\nend=1\n"), None, "no protocol at all");
    }

    #[test]
    fn messages_are_split_however_they_arrive() {
        let text = format_message(&reading_at(1)) + &format_message(&reading_at(2));
        for cut in 0..text.len() {
            let mut buffer = text[..cut].to_string();
            let mut got = Vec::new();
            while let Some(m) = take_message(&mut buffer) {
                got.push(m);
            }
            buffer.push_str(&text[cut..]);
            while let Some(m) = take_message(&mut buffer) {
                got.push(m);
            }
            assert_eq!(got.len(), 2, "cut at {cut}");
            assert_eq!(parse_message(&got[1]), Some(Message::Reading(reading_at(2))));
            assert!(buffer.is_empty());
        }
    }

    #[test]
    fn a_stalled_service_counts_as_alive_until_the_limit() {
        assert!(reading_is_current(100, 106));
        assert!(reading_is_current(100, 100 + SILENCE_LIMIT_SECS));
        assert!(!reading_is_current(100, 101 + SILENCE_LIMIT_SECS));
    }

    #[test]
    fn the_hub_numbers_each_message() {
        let hub = Hub::default();
        hub.publish(&reading_at(1));
        hub.publish(&reading_at(2));
        let latest = hub.latest.lock().unwrap();
        assert_eq!(latest.0, 2);
        assert_eq!(parse_message(&latest.1), Some(Message::Reading(reading_at(2))));
    }
}

// Installs and removes the sensor service for a portable copy. The MSI installs the same
// service itself. Here it also works as a repair, if the MSI's service has been stopped.
//
// The service runs as SYSTEM, so it runs from its own copy of monitor.exe under Program
// Files. A portable copy may be in a folder the user can write to, and running from there
// would let any program get SYSTEM rights by replacing the file.
//
// This exe is copied there too, and the service gets its own entry in Windows' installed
// apps, so it can be removed even if the portable copy that installed it is long gone.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::ERROR_SERVICE_ALREADY_RUNNING;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, REG_DWORD,
    REG_SZ,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceConfigW,
    QueryServiceStatus, StartServiceW, ENUM_SERVICE_TYPE, QUERY_SERVICE_CONFIGW, SC_ACTION,
    SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP,
    SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_STATUS,
    SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::pawnio_setup;
use crate::setup_exit;

const SERVICE_NAME: PCWSTR = w!("MonitorSensors");
const FOLDER_NAME: &str = "Monitor Sensors";
const APP_EXE: &str = "monitor.exe";
const SERVICE_EXE: &str = "monitor-sensors.exe";
const SETUP_EXE: &str = "monitor-setup.exe";
const UNINSTALL_KEY: PCWSTR =
    w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MonitorSensors");

const STOP_WAIT: Duration = Duration::from_secs(15);
const FILE_RETRY_WAIT: Duration = Duration::from_secs(5);

/// The folder for the service, only writable by admins.
fn service_folder() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("ProgramFiles")?).join(FOLDER_NAME))
}

struct Scm(SC_HANDLE);

impl Scm {
    fn open() -> Result<Self, String> {
        unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS) }
            .map(Self)
            .map_err(|e| format!("can't open the service manager: {}", e.message()))
    }
}

impl Drop for Scm {
    fn drop(&mut self) {
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

struct Service(SC_HANDLE);

impl Service {
    fn open(scm: &Scm) -> Option<Self> {
        unsafe { OpenServiceW(scm.0, SERVICE_NAME, SERVICE_ALL_ACCESS) }
            .ok()
            .map(Self)
    }

    /// The command it runs.
    fn command(&self) -> Option<String> {
        // u64s, for the alignment the structure at the start needs.
        let mut buffer = vec![0u64; 1024];
        let config = buffer.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        let size = (buffer.len() * 8) as u32;
        let mut needed = 0u32;
        unsafe {
            QueryServiceConfigW(self.0, Some(config), size, &mut needed).ok()?;
            (*config).lpBinaryPathName.to_string().ok()
        }
    }

    /// Stops it, and waits until it has.
    fn stop(&self) {
        let mut status = SERVICE_STATUS::default();
        let _ = unsafe { ControlService(self.0, SERVICE_CONTROL_STOP, &mut status) };
        let start = Instant::now();
        while start.elapsed() < STOP_WAIT {
            let stopped = unsafe { QueryServiceStatus(self.0, &mut status) }.is_ok()
                && status.dwCurrentState == SERVICE_STOPPED;
            if stopped {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log::warn!("the sensor service didn't stop in time");
    }

    fn start(&self) -> Result<(), String> {
        match unsafe { StartServiceW(self.0, None) } {
            Err(e) if e.code() != ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => {
                Err(format!("can't start the service: {}", e.message()))
            }
            _ => Ok(()),
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

/// Whether a service command runs an exe from `folder`.
fn command_runs_from(command: &str, folder: &Path) -> bool {
    let command = command.trim_start_matches('"').to_lowercase();
    command.starts_with(&format!("{}\\", folder.display()).to_lowercase())
}

pub fn run_install() -> u32 {
    log::info!("installing the sensor service {}", env!("CARGO_PKG_VERSION"));
    let restart_needed = match install_driver() {
        Ok(restart_needed) => restart_needed,
        Err((code, message)) => {
            log::error!("{message}");
            return code;
        }
    };
    if let Err(e) = install_service() {
        log::error!("{e}");
        return setup_exit::SERVICE_FAILED;
    }
    log::info!("sensor service installed, restart needed: {restart_needed}");
    if restart_needed {
        setup_exit::RESTART_WINDOWS
    } else {
        setup_exit::OK
    }
}

/// Installs PawnIO unless it's already there. `Ok(true)` if Windows has to restart first.
fn install_driver() -> Result<bool, (u32, String)> {
    if pawnio_setup::is_available() {
        log::info!("PawnIO is already installed");
        return Ok(false);
    }
    let setup = pawnio_setup::find_setup().ok_or_else(|| {
        (
            setup_exit::DRIVER_SETUP_MISSING,
            format!("{} isn't next to this exe", pawnio_setup::SETUP_FILE_NAME),
        )
    })?;
    log::info!("installing PawnIO from {}", setup.display());
    pawnio_setup::run_setup(&setup).map_err(|e| (setup_exit::DRIVER_FAILED, e))
}

fn write_string(key: HKEY, name: PCWSTR, value: &str) -> Result<(), String> {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    unsafe { RegSetValueExW(key, name, 0, REG_SZ, Some(bytes)) }
        .ok()
        .map_err(|e| format!("can't write the uninstall entry: {}", e.message()))
}

fn add_uninstall_entry(setup_exe: &Path) -> Result<(), String> {
    let mut key = HKEY::default();
    unsafe { RegCreateKeyW(HKEY_LOCAL_MACHINE, UNINSTALL_KEY, &mut key) }
        .ok()
        .map_err(|e| format!("can't create the uninstall entry: {}", e.message()))?;
    let exe_text = setup_exe.display().to_string();
    let result = (|| {
        write_string(key, w!("DisplayName"), "Monitor CPU sensors")?;
        write_string(key, w!("DisplayVersion"), env!("CARGO_PKG_VERSION"))?;
        write_string(key, w!("Publisher"), "Oleksandr Rozdaibida")?;
        write_string(key, w!("DisplayIcon"), &exe_text)?;
        write_string(key, w!("UninstallString"), &format!("\"{exe_text}\" uninstall"))?;
        for name in [w!("NoModify"), w!("NoRepair")] {
            let one = 1u32.to_le_bytes();
            unsafe { RegSetValueExW(key, name, 0, REG_DWORD, Some(&one)) }
                .ok()
                .map_err(|e| format!("can't write the uninstall entry: {}", e.message()))?;
        }
        Ok(())
    })();
    let _ = unsafe { RegCloseKey(key) };
    result
}

/// A file can stay locked for a moment after its process has stopped.
fn with_file_retries(mut action: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let start = Instant::now();
    loop {
        match action() {
            Ok(()) => return Ok(()),
            Err(_) if start.elapsed() < FILE_RETRY_WAIT => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    }
}

fn copy_retrying(from: &Path, to: &Path) -> Result<(), String> {
    with_file_retries(|| std::fs::copy(from, to).map(|_| ()))
        .map_err(|e| format!("can't copy {} to {}: {e}", from.display(), to.display()))
}

fn install_service() -> Result<(), String> {
    let scm = Scm::open()?;
    let folder = service_folder().ok_or("ProgramFiles isn't set")?;
    if let Some(existing) = Service::open(&scm) {
        let command = existing.command().unwrap_or_default();
        if !command_runs_from(&command, &folder) {
            // The MSI's service. It's the one to use, and the MSI is what updates it.
            log::info!("using the service that's already installed: {command}");
            return existing.start();
        }
        existing.stop();
    }

    let this_exe = std::env::current_exe().map_err(|e| format!("can't find this exe: {e}"))?;
    let source = this_exe.parent().ok_or("this exe has no folder")?;
    std::fs::create_dir_all(&folder).map_err(|e| format!("can't create {}: {e}", folder.display()))?;
    let service_exe = folder.join(SERVICE_EXE);
    let setup_exe = folder.join(SETUP_EXE);
    if !this_exe.starts_with(&folder) {
        copy_retrying(&source.join(APP_EXE), &service_exe)?;
        copy_retrying(&this_exe, &setup_exe)?;
    }

    let command = HSTRING::from(format!("\"{}\" --service", service_exe.display()));
    let display_name = w!("Monitor CPU sensors");
    let service = match Service::open(&scm) {
        Some(service) => {
            unsafe {
                ChangeServiceConfigW(
                    service.0,
                    ENUM_SERVICE_TYPE(SERVICE_WIN32_OWN_PROCESS.0),
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    &command,
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    display_name,
                )
            }
            .map_err(|e| format!("can't update the service: {}", e.message()))?;
            service
        }
        None => Service(
            unsafe {
                CreateServiceW(
                    scm.0,
                    SERVICE_NAME,
                    display_name,
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    &command,
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                )
            }
            .map_err(|e| format!("can't create the service: {}", e.message()))?,
        ),
    };
    describe_service(&service);
    add_uninstall_entry(&setup_exe)?;
    service.start()
}

/// Not essential, so failures here only get logged.
fn describe_service(service: &Service) {
    let description = HSTRING::from("Reads CPU temperature, frequency and power for Monitor.");
    let mut info = SERVICE_DESCRIPTIONW {
        lpDescription: windows::core::PWSTR(description.as_ptr() as *mut _),
    };
    let described = unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some(&mut info as *mut _ as *mut _),
        )
    };
    // Restart after a crash, waiting a little longer each time.
    let mut actions = [1_000u32, 10_000, 60_000].map(|delay| SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: delay,
    });
    let mut failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 60 * 60,
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
        ..Default::default()
    };
    let recovers = unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&mut failure as *mut _ as *mut _),
        )
    };
    if let Err(e) = described.and(recovers) {
        log::warn!("can't fully configure the service: {}", e.message());
    }
}

/// Removes a portable copy's service. An MSI's service is left alone, as the MSI owns it.
/// PawnIO stays, as other apps can share it, and it has its own uninstall entry.
pub fn run_uninstall() -> u32 {
    match uninstall_service() {
        Ok(()) => setup_exit::OK,
        Err(e) => {
            log::error!("{e}");
            setup_exit::SERVICE_FAILED
        }
    }
}

fn uninstall_service() -> Result<(), String> {
    let scm = Scm::open()?;
    let folder = service_folder().ok_or("ProgramFiles isn't set")?;
    if let Some(service) = Service::open(&scm) {
        let command = service.command().unwrap_or_default();
        if !command_runs_from(&command, &folder) {
            log::info!("leaving the service alone, it's the MSI's: {command}");
            return Ok(());
        }
        service.stop();
        unsafe { DeleteService(service.0) }
            .map_err(|e| format!("can't delete the service: {}", e.message()))?;
        log::info!("sensor service removed");
    }
    let _ = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, UNINSTALL_KEY) };

    let running_from_it = std::env::current_exe().is_ok_and(|exe| exe.starts_with(&folder));
    if running_from_it {
        delete_folder_after_exit(&folder)
    } else if folder.exists() {
        with_file_retries(|| std::fs::remove_dir_all(&folder))
            .map_err(|e| format!("can't delete {}: {e}", folder.display()))
    } else {
        Ok(())
    }
}

/// Uninstalling from Windows' installed apps runs the exe in the folder being deleted, and
/// a running exe can't be deleted, so a short-lived command does it once this has exited.
fn delete_folder_after_exit(folder: &Path) -> Result<(), String> {
    Command::new("cmd.exe")
        .raw_arg(format!(
            "/c ping -n 4 127.0.0.1 >nul & rmdir /s /q \"{}\"",
            folder.display()
        ))
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("can't start deleting {}: {e}", folder.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_a_service_running_from_the_portable_service_folder() {
        let folder = Path::new(r"C:\Program Files\Monitor Sensors");
        assert!(command_runs_from(
            r#""C:\Program Files\Monitor Sensors\monitor-sensors.exe" --service"#,
            folder
        ));
        assert!(command_runs_from(
            r#""c:\program files\monitor sensors\monitor-sensors.exe" --service"#,
            folder
        ));
        assert!(!command_runs_from(
            r#""C:\Program Files\Monitor\monitor.exe" --service"#,
            folder
        ));
        assert!(!command_runs_from(
            r#""C:\Program Files\Monitor Sensors Other\x.exe""#,
            folder
        ));
        assert!(!command_runs_from("", folder));
    }

    #[test]
    fn the_service_runs_from_a_folder_only_admins_can_write_to() {
        let folder = service_folder().unwrap();
        let program_files = std::env::var_os("ProgramFiles").unwrap();
        assert!(folder.starts_with(program_files));
    }
}

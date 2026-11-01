// Opening the PawnIO device. Shared with the setup crate, which checks the driver is there.

use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, StartServiceW, SC_MANAGER_CONNECT,
    SERVICE_START,
};

const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";
const DEVICE_WAIT: Duration = Duration::from_secs(3);

pub fn open_device() -> windows::core::Result<HANDLE> {
    let name: Vec<u16> = DEVICE_PATH.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            0xC000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
}

pub fn is_missing(e: &windows::core::Error) -> bool {
    e.code() == ERROR_FILE_NOT_FOUND.to_hresult() || e.code() == ERROR_PATH_NOT_FOUND.to_hresult()
}

/// The driver is set to start on demand, so it might need asking, as after a restart.
pub fn start_driver_service() {
    unsafe {
        let Ok(scm) = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) else {
            return;
        };
        if let Ok(service) = OpenServiceW(scm, w!("PawnIO"), SERVICE_START) {
            match StartServiceW(service, None) {
                Ok(()) => log::info!("started the PawnIO driver"),
                Err(e) => log::warn!("can't start the PawnIO driver: {}", e.message()),
            }
            let _ = CloseServiceHandle(service);
        }
        let _ = CloseServiceHandle(scm);
    }
}

/// The device can take a moment to appear after the driver starts.
pub fn open_device_waiting() -> Result<HANDLE, String> {
    let start = Instant::now();
    loop {
        match open_device() {
            Ok(handle) => return Ok(handle),
            Err(e) if start.elapsed() >= DEVICE_WAIT => {
                return Err(format!("can't open the PawnIO driver: {}", e.message()));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

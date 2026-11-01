// Installs the PawnIO driver with its official setup, which ships beside this exe. The
// driver stays installed, as other apps share it. It has its own entry in Windows' installed
// apps.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::pawnio_device;

pub const SETUP_FILE_NAME: &str = "PawnIO_setup.exe";
const SETUP_TIMEOUT: Duration = Duration::from_secs(120);
const SETUP_EXIT_REBOOT_REQUIRED: i32 = 3010;

/// Whether the driver is installed and running, or can be started.
pub fn is_available() -> bool {
    let opened = match pawnio_device::open_device() {
        Err(e) if pawnio_device::is_missing(&e) => {
            pawnio_device::start_driver_service();
            pawnio_device::open_device_waiting().ok()
        }
        result => result.ok(),
    };
    match opened {
        Some(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            true
        }
        None => false,
    }
}

/// PawnIO's setup ships beside the exe. A development build has it in the source tree.
pub fn find_setup() -> Option<PathBuf> {
    let beside_exe = std::env::current_exe().ok()?.parent()?.join(SETUP_FILE_NAME);
    if beside_exe.exists() {
        return Some(beside_exe);
    }
    #[cfg(debug_assertions)]
    {
        let in_source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("driver")
            .join(SETUP_FILE_NAME);
        if in_source.exists() {
            return Some(in_source);
        }
    }
    None
}

/// A folder for running setup from, removed on drop. Under Program Files because this runs
/// elevated: ordinary programs can't write there, so none can swap the file, or add a DLL
/// for it to load, as they could in a folder under the user's profile.
struct SetupFolder(PathBuf);

impl SetupFolder {
    fn create() -> Result<Self, String> {
        let base = std::env::var_os("ProgramFiles").ok_or("ProgramFiles isn't set")?;
        let folder =
            PathBuf::from(base).join(format!("Monitor PawnIO setup {}", std::process::id()));
        std::fs::create_dir_all(&folder).map_err(|e| format!("can't create setup folder: {e}"))?;
        Ok(Self(folder))
    }
}

impl Drop for SetupFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs PawnIO's setup from a copy of `setup`, and waits for the driver to be ready.
/// `Ok(true)` if Windows has to restart first.
pub fn run_setup(setup: &Path) -> Result<bool, String> {
    let folder = SetupFolder::create()?;
    let exe = folder.0.join(SETUP_FILE_NAME);
    std::fs::copy(setup, &exe).map_err(|e| format!("can't copy PawnIO setup: {e}"))?;
    let mut child = Command::new(&exe)
        .args(["-install", "-silent"])
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|e| format!("can't run PawnIO setup: {e}"))?;

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= SETUP_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("PawnIO setup didn't finish".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("can't wait for PawnIO setup: {e}")),
        }
    };
    let restart_needed = setup_result(status.code())?;
    if !restart_needed {
        let handle = pawnio_device::open_device_waiting()?;
        let _ = unsafe { CloseHandle(handle) };
    }
    log::info!("PawnIO installed, restart needed: {restart_needed}");
    Ok(restart_needed)
}

fn setup_result(code: Option<i32>) -> Result<bool, String> {
    match code {
        Some(0) => Ok(false),
        Some(SETUP_EXIT_REBOOT_REQUIRED) => Ok(true),
        Some(code) => Err(format!("PawnIO setup failed with code {code}")),
        None => Err("PawnIO setup was stopped".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_exit_codes_say_what_happened() {
        assert_eq!(setup_result(Some(0)), Ok(false));
        assert_eq!(setup_result(Some(SETUP_EXIT_REBOOT_REQUIRED)), Ok(true));
        assert!(setup_result(Some(5)).unwrap_err().contains("code 5"));
        assert!(setup_result(None).is_err());
    }
}

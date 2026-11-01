// The app's side of turning on the sensors: starts monitor-setup.exe, which does the install
// and is what Windows shows the admin prompt for.

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::setup_exit;

const SETUP_EXE: &str = "monitor-setup.exe";

fn failure_message(code: u32) -> String {
    match code {
        setup_exit::RESTART_WINDOWS => {
            "Restart Windows to finish installing the PawnIO driver.".to_string()
        }
        setup_exit::DRIVER_SETUP_MISSING => {
            "PawnIO_setup.exe is missing from Monitor's folder, so the driver can't be installed. \
             Reinstall Monitor, or unpack the whole zip."
                .to_string()
        }
        setup_exit::DRIVER_FAILED => {
            "The PawnIO driver couldn't be installed. See the setup log.".to_string()
        }
        setup_exit::SERVICE_FAILED => {
            "The sensor service couldn't be installed. See the setup log.".to_string()
        }
        code => format!("The sensor service setup failed with code {code}."),
    }
}

/// Runs the setup and waits for it, which takes a while, so this belongs on a background
/// thread. `Ok(false)` if the user declined the admin prompt.
pub fn request_install() -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find this exe: {e}"))?;
    let setup = exe
        .parent()
        .ok_or("this exe has no folder")?
        .join(SETUP_EXE);
    if !setup.exists() {
        return Err(format!(
            "{SETUP_EXE} is missing from Monitor's folder. Reinstall Monitor, or unpack the \
             whole zip."
        ));
    }
    let setup = HSTRING::from(setup.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(setup.as_ptr()),
        lpParameters: w!("install"),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        return if e.code() == ERROR_CANCELLED.to_hresult() {
            Ok(false)
        } else {
            Err(format!("can't start the setup: {}", e.message()))
        };
    }
    if info.hProcess.is_invalid() {
        return Err("the setup started without a process to wait for".to_string());
    }
    let process: HANDLE = info.hProcess;
    let mut code = 1u32;
    unsafe {
        WaitForSingleObject(process, INFINITE);
        let _ = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
    }
    match code {
        setup_exit::OK => Ok(true),
        code => Err(failure_message(code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_says_what_to_do() {
        assert!(failure_message(setup_exit::RESTART_WINDOWS).contains("Restart Windows"));
        assert!(failure_message(setup_exit::DRIVER_SETUP_MISSING).contains("PawnIO_setup.exe"));
        assert!(failure_message(setup_exit::DRIVER_FAILED).contains("PawnIO"));
        assert!(failure_message(setup_exit::SERVICE_FAILED).contains("service"));
        assert!(failure_message(77).contains("77"));
    }
}

// PawnIO kernel-driver access. PawnIO runs small signed modules, and a module only allows
// the registers it lists, so this can't be used to read or write arbitrary memory the way
// a raw ring-0 driver can.
//
// Installing the driver is monitor-setup.exe's job. Requires administrator rights.

use std::ffi::c_void;

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::Win32::System::IO::DeviceIoControl;

use super::pawnio_device::{is_missing, open_device, open_device_waiting, start_driver_service};

const DEVICE_TYPE: u32 = 41394 << 16;
const IOCTL_LOAD_MODULE: u32 = DEVICE_TYPE | (0x821 << 2);
const IOCTL_EXECUTE: u32 = DEVICE_TYPE | (0x841 << 2);
/// An execute request starts with the function name in a field of this size.
const FUNCTION_NAME_BYTES: usize = 32;

/// The lock hardware tools share for PCI config registers. A read over SMN is a separate
/// write and read of a register pair, which another app could otherwise disturb.
const PCI_LOCK_NAME: &str = r"Global\Access_PCI";
const PCI_LOCK_WAIT_MS: u32 = 100;

pub struct Driver {
    handle: HANDLE,
    pci_lock: Option<HANDLE>,
}

impl Driver {
    /// Opens the driver and loads `module`.
    pub fn open(module: &[u8]) -> Result<Driver, String> {
        let handle = match open_device() {
            Ok(handle) => handle,
            Err(e) if is_missing(&e) => {
                start_driver_service();
                open_device_waiting()?
            }
            Err(e) => return Err(format!("can't open the PawnIO driver: {}", e.message())),
        };
        let driver = Driver {
            handle,
            pci_lock: unsafe { CreateMutexW(None, false, &HSTRING::from(PCI_LOCK_NAME)) }
                .inspect_err(|e| log::warn!("no PCI lock, reading without it: {}", e.message()))
                .ok(),
        };
        driver.load_module(module)?;
        Ok(driver)
    }

    fn load_module(&self, module: &[u8]) -> Result<(), String> {
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_LOAD_MODULE,
                Some(module.as_ptr() as *const c_void),
                module.len() as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| format!("PawnIO refused the sensor module for this CPU: {}", e.message()))
    }

    fn execute(&self, function: &str, argument: u64) -> Option<u64> {
        let input = execute_input(function, argument);
        let mut output = 0u64;
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_EXECUTE,
                Some(input.as_ptr() as *const c_void),
                input.len() as u32,
                Some(&mut output as *mut u64 as *mut c_void),
                8,
                Some(&mut returned),
                None,
            )
            .ok()?;
        }
        (returned == 8).then_some(output)
    }

    /// Fails for any register the loaded module doesn't list.
    pub fn read_msr(&self, register: u32) -> Option<u64> {
        self.execute("ioctl_read_msr", register as u64)
    }

    /// Read from AMD's System Management Network.
    pub fn read_smn(&self, address: u32) -> Option<u32> {
        if let Some(lock) = self.pci_lock {
            let wait = unsafe { WaitForSingleObject(lock, PCI_LOCK_WAIT_MS) };
            if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
                return None;
            }
        }
        let value = self.execute("ioctl_read_smn", address as u64);
        if let Some(lock) = self.pci_lock {
            let _ = unsafe { ReleaseMutex(lock) };
        }
        value.map(|v| v as u32)
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
            if let Some(lock) = self.pci_lock {
                let _ = CloseHandle(lock);
            }
        }
    }
}

fn execute_input(function: &str, argument: u64) -> Vec<u8> {
    let mut input = vec![0u8; FUNCTION_NAME_BYTES];
    let name = function.as_bytes();
    // The last byte stays zero, to end the name.
    let length = name.len().min(FUNCTION_NAME_BYTES - 1);
    input[..length].copy_from_slice(&name[..length]);
    input.extend_from_slice(&argument.to_le_bytes());
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_requests_start_with_the_name_then_the_argument() {
        let input = execute_input("ioctl_read_msr", 0xC001_029B);
        assert_eq!(input.len(), FUNCTION_NAME_BYTES + 8);
        assert_eq!(&input[..14], b"ioctl_read_msr");
        assert!(input[14..FUNCTION_NAME_BYTES].iter().all(|&b| b == 0));
        assert_eq!(input[FUNCTION_NAME_BYTES..], 0xC001_029Bu64.to_le_bytes());
    }

    #[test]
    fn long_function_names_still_end_with_a_zero() {
        let input = execute_input(&"x".repeat(50), 0);
        assert_eq!(input.len(), FUNCTION_NAME_BYTES + 8);
        assert_eq!(input[FUNCTION_NAME_BYTES - 2], b'x');
        assert_eq!(input[FUNCTION_NAME_BYTES - 1], 0);
    }

    #[test]
    fn the_ioctl_codes_match_the_driver() {
        assert_eq!(IOCTL_LOAD_MODULE, 0xA1B2_2084);
        assert_eq!(IOCTL_EXECUTE, 0xA1B2_2104);
    }
}

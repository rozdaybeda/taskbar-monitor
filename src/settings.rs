// The app's per-user choices: starting with Windows, and checking for updates.
//
// Starting with Windows lives only in the user's Run key, where Windows looks for it. So it
// needs no admin rights, and a copy can tell whether it's the one that will start.
//
// The first time a user runs an installed copy, these start from what was chosen in the MSI,
// which it leaves in HKLM. After that they're the user's own.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    REG_SZ, RRF_RT_REG_SZ,
};

use crate::logging;

const SETTINGS_FILE: &str = "settings.txt";
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("Monitor");
const MACHINE_KEY: PCWSTR = w!(r"Software\Monitor");

static CHECK_UPDATES: AtomicBool = AtomicBool::new(true);

/// Where the first run's choices come from.
#[derive(Debug, PartialEq, Default)]
struct MachineDefaults {
    install_folder: Option<String>,
    auto_start: Option<String>,
    check_updates: Option<String>,
}

#[derive(Debug, PartialEq)]
struct FirstRun {
    auto_start: bool,
    check_updates: bool,
}

/// The MSI's choices only apply to the copy it installed. A portable or development copy on
/// the same PC shouldn't add itself to the user's startup programs.
fn first_run_choices(defaults: &MachineDefaults, exe_folder: &Path) -> FirstRun {
    let installed = defaults
        .install_folder
        .as_deref()
        .is_some_and(|folder| same_folder(folder, exe_folder));
    FirstRun {
        auto_start: installed && defaults.auto_start.as_deref() == Some("1"),
        check_updates: !installed || defaults.check_updates.as_deref() != Some("0"),
    }
}

fn same_folder(text: &str, folder: &Path) -> bool {
    let normalize = |s: &str| s.trim_end_matches('\\').to_lowercase();
    normalize(text) == normalize(&folder.display().to_string())
}

fn parse_check_updates(text: &str) -> Option<bool> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("check_updates="))
        .map(|value| value != "0")
}

fn format_settings(check_updates: bool) -> String {
    format!("check_updates={}\n", check_updates as u8)
}

fn settings_path() -> Option<PathBuf> {
    Some(logging::app_data_dir()?.join(SETTINGS_FILE))
}

fn save() {
    let Some(path) = settings_path() else {
        return;
    };
    if let Err(e) = std::fs::write(&path, format_settings(check_updates())) {
        log::warn!("can't save {}: {e}", path.display());
    }
}

fn read_string(root: HKEY, key: PCWSTR, name: PCWSTR) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    let result = unsafe {
        RegGetValueW(
            root,
            key,
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if result.is_err() {
        return None;
    }
    let length = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buffer[..length]))
}

fn machine_defaults() -> MachineDefaults {
    let read = |name| read_string(HKEY_LOCAL_MACHINE, MACHINE_KEY, name);
    MachineDefaults {
        install_folder: read(w!("InstallFolder")),
        auto_start: read(w!("AutoStart")),
        check_updates: read(w!("CheckUpdates")),
    }
}

/// Loads the settings, or sets them up if this is the user's first run.
pub fn init() {
    if let Some(saved) = settings_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        CHECK_UPDATES.store(parse_check_updates(&saved).unwrap_or(true), Ordering::SeqCst);
    } else {
        let exe = std::env::current_exe().ok();
        let folder = exe.as_deref().and_then(Path::parent).unwrap_or(Path::new(""));
        let first_run = first_run_choices(&machine_defaults(), folder);
        log::info!("first run for this user: {first_run:?}");
        CHECK_UPDATES.store(first_run.check_updates, Ordering::SeqCst);
        if first_run.auto_start {
            set_auto_start(true);
        }
        save();
    }
    log::info!(
        "start with Windows: {}, check for updates: {}",
        auto_start(),
        check_updates()
    );
}

pub fn check_updates() -> bool {
    CHECK_UPDATES.load(Ordering::SeqCst)
}

pub fn set_check_updates(on: bool) {
    log::info!("check for updates: {on}");
    CHECK_UPDATES.store(on, Ordering::SeqCst);
    save();
}

fn own_run_command() -> Option<String> {
    Some(format!("\"{}\"", std::env::current_exe().ok()?.display()))
}

/// Whether this copy of the app starts with Windows. Another copy being set to start, such
/// as the installed one while this is a portable one, doesn't count.
pub fn auto_start() -> bool {
    let current = read_string(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
    current.is_some_and(|command| {
        own_run_command().is_some_and(|own| own.eq_ignore_ascii_case(&command))
    })
}

pub fn set_auto_start(on: bool) {
    log::info!("start with Windows: {on}");
    let result = if on {
        let Some(command) = own_run_command() else {
            return;
        };
        let wide: Vec<u16> = HSTRING::from(command.as_str()).as_wide().to_vec();
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(wide.as_ptr() as *const c_void),
                ((wide.len() + 1) * 2) as u32,
            )
        }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE) }
    };
    if let Err(e) = result.ok() {
        log::warn!("can't change starting with Windows: {}", e.message());
    }
}

/// For `--remove-autostart`, which the MSI runs when uninstalling.
pub fn remove_own_auto_start() {
    if auto_start() {
        set_auto_start(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults(auto_start: &str, check_updates: &str) -> MachineDefaults {
        MachineDefaults {
            install_folder: Some(r"C:\Program Files\Monitor\".to_string()),
            auto_start: Some(auto_start.to_string()),
            check_updates: Some(check_updates.to_string()),
        }
    }

    #[test]
    fn the_installed_copy_starts_from_the_msi_choices() {
        let folder = Path::new(r"c:\program files\monitor");
        for (auto_start, check_updates, want_auto_start, want_check_updates) in [
            ("1", "1", true, true),
            ("0", "1", false, true),
            ("1", "0", true, false),
            ("0", "0", false, false),
        ] {
            assert_eq!(
                first_run_choices(&defaults(auto_start, check_updates), folder),
                FirstRun {
                    auto_start: want_auto_start,
                    check_updates: want_check_updates
                }
            );
        }
    }

    #[test]
    fn other_copies_ignore_the_msi_choices() {
        let choices = first_run_choices(&defaults("1", "0"), Path::new(r"D:\Tools\Monitor"));
        assert_eq!(choices, FirstRun { auto_start: false, check_updates: true });
        assert_eq!(
            first_run_choices(&MachineDefaults::default(), Path::new(r"C:\Program Files\Monitor")),
            FirstRun { auto_start: false, check_updates: true }
        );
    }

    #[test]
    fn folders_compare_regardless_of_case_and_a_trailing_slash() {
        assert!(same_folder(r"C:\Program Files\Monitor\", Path::new(r"c:\program files\monitor")));
        assert!(!same_folder(r"C:\Program Files\Monitor", Path::new(r"C:\Program Files\Monitor2")));
    }

    #[test]
    fn settings_round_trip() {
        assert_eq!(parse_check_updates(&format_settings(true)), Some(true));
        assert_eq!(parse_check_updates(&format_settings(false)), Some(false));
        assert_eq!(parse_check_updates("other=1\r\ncheck_updates=0\r\n"), Some(false));
        assert_eq!(parse_check_updates(""), None);
    }
}

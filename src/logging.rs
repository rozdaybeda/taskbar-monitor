// A small file logger for the `log` macros, writing to %APPDATA%\Monitor. The app has
// no console, so without this, failures and even panics would leave no trace.
//
// Each process gets its own file, named with its PID. The sensor service keeps its logs
// next to its own exe instead, as it doesn't run as the user.
//
// Size is limited at three levels. A line can't exceed MAX_LINE_BYTES. When a file
// reaches MAX_LOG_BYTES it's kept as "<name>.old" and a new one is started, which caps a
// process at about twice that however long it runs. And each start removes old logs, by
// age, count and total size.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use log::{Level, LevelFilter, Log, Metadata, Record};
use windows::Win32::System::SystemInformation::GetLocalTime;

pub const GUI_LOG: &str = "monitor";
pub const SERVICE_LOG: &str = "service";
pub const SETUP_LOG: &str = "setup";
/// From versions that used a helper process, so that cleaning up removes those logs too.
const OLD_HELPER_LOG: &str = "helper";

const MAX_LOG_BYTES: u64 = 512 * 1024;
const MAX_LINE_BYTES: usize = 32 * 1024;
const MAX_LOG_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_LOG_FILES: usize = 12;
const MAX_LOGS_TOTAL_BYTES: u64 = 10 * 1024 * 1024;
/// This file is shared with the setup crate, whose logs count as this app's too.
const OWN_CRATES: [&str; 2] = ["monitor", "monitor_setup"];

/// A file next to the exe that makes this a portable copy.
const PORTABLE_MARKER: &str = "portable.txt";

fn portable_data_dir(exe_dir: &Path) -> Option<PathBuf> {
    exe_dir
        .join(PORTABLE_MARKER)
        .exists()
        .then(|| exe_dir.join("data"))
}

fn own_portable_data_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    portable_data_dir(exe.parent()?)
}

pub fn is_portable() -> bool {
    own_portable_data_dir().is_some()
}

/// Where the app keeps its settings and logs: a folder next to the exe for a portable
/// copy, so it leaves nothing behind on the machine, otherwise %APPDATA%\Monitor.
pub fn app_data_dir() -> Option<PathBuf> {
    if let Some(dir) = own_portable_data_dir() {
        return Some(dir);
    }
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("Monitor"))
}

/// Everything of ours from Info up; only problems from libraries such as eframe.
fn wanted(target: &str, level: Level) -> bool {
    let crate_name = target.split("::").next().unwrap_or(target);
    let ours = OWN_CRATES.contains(&crate_name);
    level <= if ours { Level::Info } else { Level::Warn }
}

fn format_line(timestamp: &str, level: Level, target: &str, message: &str) -> String {
    let module = OWN_CRATES
        .iter()
        .find_map(|name| target.strip_prefix(*name)?.strip_prefix("::"))
        .unwrap_or(target);
    let mut line = format!("{timestamp} {:<5} [{module}] {message}", level.as_str());
    if line.len() > MAX_LINE_BYTES {
        let mut end = MAX_LINE_BYTES;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str(" [cut]");
    }
    line.push('\n');
    line
}

fn log_file_name(kind: &str, pid: u32) -> String {
    format!("{kind}_{pid}.log")
}

/// Only files this app made, so cleaning up can't touch anything else in the folder.
/// Includes the "monitor.log" name that older builds used.
fn is_log_file(name: &str) -> bool {
    let is_ours = [GUI_LOG, SERVICE_LOG, SETUP_LOG, OLD_HELPER_LOG]
        .iter()
        .any(|kind| name.starts_with(kind));
    is_ours && (name.ends_with(".log") || name.ends_with(".log.old"))
}

/// Which logs to delete, given (name, size, modified) for each. Newest are kept first,
/// until there are too many or they take too much space. `current` always stays.
fn logs_to_delete(
    mut logs: Vec<(String, u64, SystemTime)>,
    current: &str,
    now: SystemTime,
) -> Vec<String> {
    logs.sort_by(|a, b| b.2.cmp(&a.2));
    let mut delete = Vec::new();
    let mut kept_files = 1; // the current log
    let mut kept_bytes = 0;
    for (name, size, modified) in logs {
        if name == current {
            continue;
        }
        let too_old = now.duration_since(modified).is_ok_and(|age| age > MAX_LOG_AGE);
        if too_old
            || kept_files >= MAX_LOG_FILES
            || kept_bytes + size > MAX_LOGS_TOTAL_BYTES
        {
            delete.push(name);
        } else {
            kept_files += 1;
            kept_bytes += size;
        }
    }
    delete
}

fn clean_old_logs(dir: &Path, current: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let logs = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().ok()?;
            is_log_file(&name).then_some((name, meta.len(), meta.modified().ok()?))
        })
        .collect();
    for name in logs_to_delete(logs, current, SystemTime::now()) {
        let _ = fs::remove_file(dir.join(name));
    }
}

fn now() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// An append-only log file that starts over once it gets too big.
struct LogFile {
    path: PathBuf,
    max_bytes: u64,
    file: Option<File>,
}

impl LogFile {
    fn new(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            file: None,
        }
    }

    fn old_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(".old");
        PathBuf::from(name)
    }

    /// Logging must never take the app down, so failures here are ignored.
    fn write_line(&mut self, line: &str) {
        match fs::metadata(&self.path) {
            Ok(meta) if meta.len() >= self.max_bytes => {
                self.file = None; // a file can't be renamed while it's open
                let _ = fs::rename(&self.path, Self::old_path(&self.path));
            }
            Ok(_) => {}
            // Deleted while open, say by hand: writes would go nowhere, so reopen.
            Err(_) => self.file = None,
        }
        if self.file.is_none() {
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok();
        }
        if let Some(file) = &mut self.file {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

struct FileLogger {
    file: Mutex<LogFile>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        wanted(metadata.target(), metadata.level())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format_line(
            &now(),
            record.level(),
            record.target(),
            &record.args().to_string(),
        );
        if let Ok(mut file) = self.file.lock() {
            file.write_line(&line);
        }
    }

    fn flush(&self) {}
}

fn install(path: PathBuf) {
    let logger: &'static FileLogger = Box::leak(Box::new(FileLogger {
        file: Mutex::new(LogFile::new(path, MAX_LOG_BYTES)),
    }));
    if log::set_logger(logger).is_err() {
        return;
    }
    log::set_max_level(LevelFilter::Info);

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(
            "panic: {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        previous(info);
    }));
}

/// Start logging this process to a file in the app's data folder, and clean up old
/// logs. If that isn't possible the app carries on without logs.
pub fn init(kind: &str) {
    if let Some(dir) = app_data_dir() {
        init_in(&dir, kind);
    }
}

/// The same, in a given folder.
pub fn init_in(dir: &Path, kind: &str) {
    if fs::create_dir_all(dir).is_ok() {
        let name = log_file_name(kind, std::process::id());
        clean_old_logs(dir, &name);
        install(dir.join(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("monitor_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn keeps_our_info_but_only_problems_from_libraries() {
        assert!(wanted("monitor", Level::Info));
        assert!(wanted("monitor::tray", Level::Info));
        assert!(!wanted("monitor::tray", Level::Debug));
        assert!(wanted("monitor_setup::service", Level::Info));
        assert!(!wanted("monitoring", Level::Info));
        assert!(!wanted("eframe::native", Level::Info));
        assert!(wanted("eframe::native", Level::Warn));
        assert!(!wanted("monitoring_lib", Level::Info));
    }

    #[test]
    fn lines_show_time_level_module_and_message() {
        assert_eq!(
            format_line("2026-11-01 15:40:03.123", Level::Warn, "monitor::tray", "hello"),
            "2026-11-01 15:40:03.123 WARN  [tray] hello\n"
        );
    }

    #[test]
    fn a_marker_file_makes_a_copy_portable() {
        let dir = temp_dir("portable");
        assert_eq!(portable_data_dir(&dir), None);
        fs::write(dir.join(PORTABLE_MARKER), "").unwrap();
        assert_eq!(portable_data_dir(&dir), Some(dir.join("data")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_names_include_the_pid() {
        assert_eq!(log_file_name(GUI_LOG, 1234), "monitor_1234.log");
        assert_eq!(log_file_name(SERVICE_LOG, 99), "service_99.log");
    }

    #[test]
    fn huge_lines_are_cut_without_splitting_a_character() {
        let line = format_line("t", Level::Info, "monitor", &"é".repeat(MAX_LINE_BYTES));
        assert!(line.len() <= MAX_LINE_BYTES + 16, "{}", line.len());
        assert!(line.ends_with(" [cut]\n"));
    }

    #[test]
    fn only_our_logs_count_as_logs() {
        assert!(is_log_file("monitor_12.log"));
        assert!(is_log_file("helper_12.log.old"));
        assert!(is_log_file("monitor.log"));
        assert!(!is_log_file("widget.txt"));
        assert!(!is_log_file("notes.log"));
        assert!(!is_log_file("monitor_12.txt"));
    }

    fn entry(name: &str, size: u64, minutes_ago: u64, now: SystemTime) -> (String, u64, SystemTime) {
        (name.to_string(), size, now - Duration::from_secs(minutes_ago * 60))
    }

    #[test]
    fn logs_past_the_age_limit_go() {
        let now = SystemTime::now();
        let week = 7 * 24 * 60;
        let logs = vec![
            entry("fresh.log", 10, 1, now),
            entry("old.log", 10, week + 1, now),
        ];
        assert_eq!(logs_to_delete(logs, "current.log", now), vec!["old.log"]);
    }

    #[test]
    fn only_the_newest_files_are_kept() {
        let now = SystemTime::now();
        let logs: Vec<_> = (0..20)
            .map(|i| entry(&format!("{i}.log"), 10, i, now))
            .collect();
        let deleted = logs_to_delete(logs, "current.log", now);
        // Room for 11 besides the current log, so 0..=10 stay.
        assert_eq!(deleted.len(), 9);
        assert!(deleted.contains(&"11.log".to_string()));
        assert!(!deleted.contains(&"10.log".to_string()));
    }

    #[test]
    fn total_size_is_limited_and_the_current_log_always_stays() {
        let now = SystemTime::now();
        let mb = 1024 * 1024;
        let logs = vec![
            entry("current.log", 50 * mb, 500, now),
            entry("a.log", 6 * mb, 1, now),
            entry("b.log", 6 * mb, 2, now),
            entry("c.log", 1 * mb, 3, now),
        ];
        assert_eq!(logs_to_delete(logs, "current.log", now), vec!["b.log"]);
    }

    #[test]
    fn cleaning_a_real_folder_leaves_other_files_alone() {
        let dir = temp_dir("clean");
        for i in 0..MAX_LOG_FILES + 5 {
            fs::write(dir.join(format!("helper_{i}.log")), "x").unwrap();
        }
        fs::write(dir.join("widget.txt"), "settings").unwrap();
        clean_old_logs(&dir, "monitor_1.log");
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"widget.txt".to_string()));
        let logs = names.iter().filter(|n| is_log_file(n)).count();
        assert_eq!(logs, MAX_LOG_FILES - 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_deleted_while_open_is_recreated() {
        let dir = temp_dir("recreate");
        let path = dir.join("test.log");
        let mut log = LogFile::new(path.clone(), 1000);
        log.write_line("one\n");
        fs::remove_file(&path).unwrap();
        log.write_line("two\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "two\n");
        drop(log);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_log_is_kept_as_old_and_a_new_one_started() {
        let dir = temp_dir("rotate");
        let path = dir.join("test.log");
        let mut log = LogFile::new(path.clone(), 10);
        log.write_line("first line\n");
        log.write_line("second\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");
        assert_eq!(
            fs::read_to_string(dir.join("test.log.old")).unwrap(),
            "first line\n"
        );
        drop(log);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn appends_across_restarts() {
        let dir = temp_dir("append");
        let path = dir.join("test.log");
        LogFile::new(path.clone(), 1000).write_line("run one\n");
        LogFile::new(path.clone(), 1000).write_line("run two\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "run one\nrun two\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn panics_and_log_calls_reach_the_file() {
        let dir = temp_dir("panic");
        let path = dir.join("test.log");
        install(path.clone());
        log::info!("hello from a test");
        let _ = std::panic::catch_unwind(|| panic!("deliberate test panic"));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("hello from a test"), "{text}");
        assert!(text.contains("panic:"), "{text}");
        assert!(text.contains("deliberate test panic"), "{text}");
    }
}

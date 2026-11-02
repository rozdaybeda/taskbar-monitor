// Checks for a newer release: shortly after starting, then once a day, unless the user has
// turned that off, and whenever they press the main window's button. This only tells the
// user. Installing is theirs to choose, by opening the download link.
//
// The check reads a small text file from the URL set by MONITOR_UPDATE_URL at build time.
// Release builds point it at the latest GitHub release. Builds without a URL never touch
// the network.
//
//     version=0.2.0
//     url=https://github.com/<owner>/<repo>/releases/download/v0.2.0/taskbar-monitor-0.2.0.msi

use std::sync::Mutex;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Networking::WinHttp::*;

const UPDATE_URL: Option<&str> = option_env!("MONITOR_UPDATE_URL");
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// How soon turning the checks back on takes effect.
const SETTING_POLL: Duration = Duration::from_secs(60);
const TIMEOUT_MS: i32 = 15_000;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub version: String,
    pub url: String,
}

/// What the main window's button shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Checking,
    UpToDate,
    Failed(String),
    Found(Update),
}

static AVAILABLE: Mutex<Option<Update>> = Mutex::new(None);
static STATUS: Mutex<Status> = Mutex::new(Status::Idle);

/// The newer release found by the last check, if any, while checks are on.
pub fn available() -> Option<Update> {
    AVAILABLE
        .lock()
        .unwrap()
        .clone()
        .filter(|_| crate::settings::check_updates())
}

/// A found update, or else how the last check from the button went. A manual check counts
/// even with the automatic ones off, since the user asked for it.
pub fn status() -> Status {
    match available() {
        Some(update) => Status::Found(update),
        None => STATUS.lock().unwrap().clone(),
    }
}

/// The button's text and hover text.
pub fn button_text(status: &Status) -> (String, String) {
    let current = concat!("You have version ", env!("CARGO_PKG_VERSION"));
    match status {
        Status::Idle => ("Check for updates".to_string(), current.to_string()),
        Status::Checking => ("Checking…".to_string(), current.to_string()),
        Status::UpToDate => (
            "Up to date".to_string(),
            format!("{current}, the latest. Click to check again."),
        ),
        Status::Failed(error) => (
            "Check failed".to_string(),
            format!("Couldn't check for updates ({error}). Click to try again."),
        ),
        Status::Found(update) => (
            format!("Update to {}", update.version),
            format!("{current}. Click to download {}.", update.version),
        ),
    }
}

/// Checks now, in the background, for the main window's button.
pub fn check_now() {
    let Some(url) = UPDATE_URL.filter(|_| can_check()) else {
        return;
    };
    {
        let mut status = STATUS.lock().unwrap();
        if *status == Status::Checking {
            return;
        }
        *status = Status::Checking;
    }
    std::thread::spawn(move || {
        log::info!("update check from the button");
        let result = check(url);
        let status = match result {
            Ok(Some(update)) => {
                log::info!("update available: {} at {}", update.version, update.url);
                *AVAILABLE.lock().unwrap() = Some(update.clone());
                Status::Found(update)
            }
            Ok(None) => Status::UpToDate,
            Err(e) => {
                log::warn!("update check failed: {e}");
                Status::Failed(e)
            }
        };
        *STATUS.lock().unwrap() = status;
    });
}

/// Whether this copy can check at all, so whether the setting means anything. The update
/// link leads to an installer, which isn't how a portable copy gets updated.
pub fn can_check() -> bool {
    !crate::logging::is_portable() && UPDATE_URL.is_some_and(|url| !url.is_empty())
}

fn check_is_due(checks_on: bool, since_last: Option<Duration>) -> bool {
    checks_on && since_last.is_none_or(|elapsed| elapsed >= CHECK_INTERVAL)
}

/// Starts checking in the background. `on_found` runs when a new version first shows up.
pub fn start(on_found: fn()) {
    let url = match UPDATE_URL {
        Some(url) if can_check() => url,
        _ => {
            log::info!("update checks not available: a portable copy, or no update URL");
            return;
        }
    };
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_CHECK_DELAY);
        let mut last_check: Option<std::time::Instant> = None;
        loop {
            let since_last = last_check.map(|t| t.elapsed());
            if !check_is_due(crate::settings::check_updates(), since_last) {
                std::thread::sleep(SETTING_POLL);
                continue;
            }
            last_check = Some(std::time::Instant::now());
            match check(url) {
                Ok(Some(update)) => {
                    let is_new = AVAILABLE.lock().unwrap().as_ref() != Some(&update);
                    if is_new {
                        log::info!("update available: {} at {}", update.version, update.url);
                        *AVAILABLE.lock().unwrap() = Some(update);
                        on_found();
                    }
                }
                Ok(None) => log::info!("update check: {} is current", env!("CARGO_PKG_VERSION")),
                Err(e) => log::warn!("update check failed: {e}"),
            }
        }
    });
}

fn check(url: &str) -> Result<Option<Update>, String> {
    let body = fetch(url)?;
    let update = parse_manifest(&body).ok_or("the update file isn't in the expected format")?;
    Ok(is_newer(&update.version, env!("CARGO_PKG_VERSION")).then_some(update))
}

/// The link gets opened by the shell, so anything other than a web address is refused.
fn parse_manifest(text: &str) -> Option<Update> {
    let mut version = None;
    let mut url = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("version=") {
            version = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("url=") {
            url = Some(v.trim().to_string());
        }
    }
    let version = version.filter(|v| parse_version(v).is_some())?;
    let url = url.filter(|u| u.starts_with("https://") && !u.contains(char::is_whitespace))?;
    Some(Update { version, url })
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim_start_matches('v').split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

#[derive(Debug, PartialEq)]
struct Target {
    secure: bool,
    host: String,
    port: u16,
    path: String,
}

/// Plain http is only for testing against this machine: a check over it elsewhere could
/// be tampered with on the way, and its link is then what the user gets sent to.
fn parse_url(url: &str) -> Option<Target> {
    let (secure, rest) = match url.split_once("://")? {
        ("https", rest) => (true, rest),
        ("http", rest) => (false, rest),
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() || (!secure && host != "127.0.0.1" && host != "localhost") {
        return None;
    }
    Some(Target {
        secure,
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

struct Handle(*mut core::ffi::c_void);

impl Handle {
    fn new(raw: *mut core::ffi::c_void, what: &str) -> Result<Self, String> {
        if raw.is_null() {
            Err(format!("{what}: {}", windows::core::Error::from_win32().message()))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

fn fetch(url: &str) -> Result<String, String> {
    let target = parse_url(url).ok_or("the update URL isn't valid")?;
    // Windows has no text for most WinHTTP errors, leaving only the code.
    let failed = |what: &str, e: windows::core::Error| match e.message() {
        message if message.is_empty() => format!("{what}: error {:#x}", e.code().0),
        message => format!("{what}: {message}"),
    };
    unsafe {
        let session = Handle::new(
            WinHttpOpen(
                &HSTRING::from(concat!("Monitor/", env!("CARGO_PKG_VERSION"))),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                None,
                None,
                0,
            ),
            "open",
        )?;
        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS)
            .map_err(|e| failed("timeouts", e))?;
        let connection = Handle::new(
            WinHttpConnect(session.0, &HSTRING::from(target.host.as_str()), target.port, 0),
            "connect",
        )?;
        let flags = if target.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = Handle::new(
            WinHttpOpenRequest(
                connection.0,
                &HSTRING::from("GET"),
                &HSTRING::from(target.path.as_str()),
                None,
                None,
                std::ptr::null(),
                flags,
            ),
            "request",
        )?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|e| failed("send", e))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| failed("receive", e))?;

        let mut status = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            None,
            Some(&mut status as *mut u32 as *mut _),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| failed("status", e))?;
        if status != 200 {
            return Err(format!("the server answered {status}"));
        }

        let mut body = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr() as *mut _, chunk.len() as u32, &mut read)
                .map_err(|e| failed("read", e))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_MANIFEST_BYTES {
                return Err("the update file is too big".to_string());
            }
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_only_run_when_on_and_due() {
        let day = CHECK_INTERVAL;
        let minute = Duration::from_secs(60);
        assert!(check_is_due(true, None), "never checked");
        assert!(check_is_due(true, Some(day)));
        assert!(!check_is_due(true, Some(minute)));
        assert!(!check_is_due(false, None));
        assert!(!check_is_due(false, Some(day)));
    }

    #[test]
    fn the_button_says_what_happened_and_what_a_click_does() {
        let label = |status: Status| button_text(&status).0;
        assert_eq!(label(Status::Idle), "Check for updates");
        assert_eq!(label(Status::UpToDate), "Up to date");
        let found = Update {
            version: "9.9.9".to_string(),
            url: "https://example.com/a.msi".to_string(),
        };
        assert_eq!(label(Status::Found(found.clone())), "Update to 9.9.9");
        assert!(button_text(&Status::Found(found)).1.contains("download 9.9.9"));
        let failed = button_text(&Status::Failed("the server answered 404".to_string()));
        assert_eq!(failed.0, "Check failed");
        assert_eq!(
            failed.1,
            "Couldn't check for updates (the server answered 404). Click to try again."
        );
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"), "not compared as text");
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("1.2", "0.1.0"));
    }

    #[test]
    fn reads_a_manifest_and_tolerates_windows_line_endings() {
        assert_eq!(
            parse_manifest("version=0.2.0\r\nurl=https://example.com/a.msi\r\n"),
            Some(Update {
                version: "0.2.0".to_string(),
                url: "https://example.com/a.msi".to_string()
            })
        );
    }

    #[test]
    fn refuses_links_the_shell_could_run() {
        for url in [
            "url=file:///C:/Windows/System32/calc.exe",
            "url=C:\\Windows\\System32\\calc.exe",
            "url=http://example.com/a.msi",
            "url=https://example.com/a b.msi",
            "",
        ] {
            assert_eq!(parse_manifest(&format!("version=1.0.0\n{url}\n")), None, "{url}");
        }
        assert_eq!(parse_manifest("url=https://example.com/a.msi\n"), None);
        assert_eq!(parse_manifest("<html>not a manifest</html>"), None);
    }

    #[test]
    fn splits_urls() {
        assert_eq!(
            parse_url("https://example.com/releases/latest.txt"),
            Some(Target {
                secure: true,
                host: "example.com".to_string(),
                port: 443,
                path: "/releases/latest.txt".to_string()
            })
        );
        let local = parse_url("http://127.0.0.1:8123/x").unwrap();
        assert_eq!((local.secure, local.port), (false, 8123));
        assert_eq!(parse_url("https://example.com").unwrap().path, "/");
    }

    /// Uses the real network, so not part of a normal test run:
    /// cargo test fetch_over_real_https -- --ignored --nocapture
    #[test]
    #[ignore]
    fn fetch_over_real_https() {
        let body = fetch("https://example.com/").unwrap();
        println!("HTTPS OK, {} bytes", body.len());
        assert!(body.contains("Example Domain"));
        assert_eq!(parse_manifest(&body), None, "a web page isn't a manifest");
        let missing = fetch("https://example.com/no-such-file.txt");
        println!("HTTPS missing file: {missing:?}");
    }

    /// Release downloads redirect twice, to another host, as the update check will rely on.
    /// Uses the real network, so not part of a normal test run:
    /// cargo test follows_github_release_redirects -- --ignored --nocapture
    #[test]
    #[ignore]
    fn follows_github_release_redirects() {
        // A real file over the size limit: getting that far means the redirects worked.
        let result = fetch(
            "https://github.com/namazso/PawnIO.Modules/releases/latest/download/release_0_2_11.zip",
        );
        println!("GitHub release download: {result:?}");
        assert_eq!(result.unwrap_err(), "the update file is too big");
        let missing = fetch("https://github.com/namazso/PawnIO.Modules/releases/latest/download/latest.txt");
        println!("GitHub missing file: {missing:?}");
        assert!(missing.unwrap_err().contains("404"));
    }

    #[test]
    fn plain_http_only_works_for_this_machine() {
        assert_eq!(parse_url("http://example.com/latest.txt"), None);
        assert_eq!(parse_url("ftp://example.com/latest.txt"), None);
        assert_eq!(parse_url("not a url"), None);
    }
}

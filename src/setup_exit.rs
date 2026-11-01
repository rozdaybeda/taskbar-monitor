// Exit codes of monitor-setup.exe, which is how the app and the .bat files learn what
// happened. Shared with the setup crate, and repeated in the .bat files in installer\.

pub const OK: u32 = 0;
pub const RESTART_WINDOWS: u32 = 10;
pub const DRIVER_SETUP_MISSING: u32 = 11;
pub const DRIVER_FAILED: u32 = 12;
pub const SERVICE_FAILED: u32 = 13;
pub const USAGE: u32 = 14;

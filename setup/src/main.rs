// monitor-setup.exe: see the library for what this does. It's split this way because this
// exe's manifest requires admin rights, which Windows would also apply to its test build.
#![windows_subsystem = "windows"]

fn main() {
    let command = std::env::args().nth(1).unwrap_or_default();
    std::process::exit(monitor_setup::run(&command) as i32);
}

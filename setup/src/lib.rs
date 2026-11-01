// Installs and removes the sensor service and the PawnIO driver for a portable copy of
// Monitor, so that monitor.exe has no install code and needs no admin rights.
//
//     monitor-setup.exe install      sets up the service and driver
//     monitor-setup.exe uninstall    removes the service; the driver stays
//
// The result goes back as the exit code (see `setup_exit`). The exe has no console, so
// nothing flashes up when the app starts it.

#[allow(dead_code)]
#[path = "../../src/logging.rs"]
mod logging;
#[allow(dead_code)]
#[path = "../../src/sensors/pawnio_device.rs"]
mod pawnio_device;
mod pawnio_setup;
mod service;
#[allow(dead_code)]
#[path = "../../src/setup_exit.rs"]
mod setup_exit;

/// Runs a command and returns the exit code.
pub fn run(command: &str) -> u32 {
    logging::init(logging::SETUP_LOG);
    match command {
        "install" => service::run_install(),
        "uninstall" => service::run_uninstall(),
        other => {
            log::error!("unknown command {other:?}, expected install or uninstall");
            setup_exit::USAGE
        }
    }
}

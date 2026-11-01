// Native sensor sampler on a background thread, writing into `Shared`.
//
// CPU temperature / clock / power need the PawnIO kernel driver (admin), so a Windows
// service reads it and sends the values over a named pipe (see `service`). The app stays
// unelevated. Everything else (load, memory, GPU, device specs) works without the service.

mod adl;
pub mod bench;
mod cpu;
pub mod deviceinfo;
mod gpu;
mod nvml;
mod pawnio;
mod pawnio_device;
pub mod service;
pub mod setup_client;
mod sysload;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use deviceinfo::DeviceInfo;

use crate::{GpuState, Shared};

/// Why the last attempt to install the sensor service failed.
static SETUP_FAILURE: Mutex<Option<String>> = Mutex::new(None);

pub fn set_setup_failure(failure: Option<String>) {
    *SETUP_FAILURE.lock().unwrap() = failure;
}

pub struct Sampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Sampler {
    pub fn start(shared: Arc<Mutex<Shared>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::spawn(move || run(shared, stop_thread));
        Sampler {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

const SAMPLE_PERIOD: Duration = Duration::from_millis(500);
/// Readings averaged into each history point: one point per second.
const SAMPLES_PER_POINT: u32 = 2;

/// Averages the load readings since the last history point. Load readings cover
/// the time since the previous reading, so averaging them accounts for the whole
/// second and short spikes still show up.
#[derive(Clone, Default)]
struct LoadWindow {
    sum: f32,
    count: u32,
}

impl LoadWindow {
    fn add(&mut self, reading: Option<f32>) {
        if let Some(v) = reading {
            self.sum += v;
            self.count += 1;
        }
    }

    /// The average so far, then starts over. `None` if there were no readings.
    fn take(&mut self) -> Option<f32> {
        let window = std::mem::take(self);
        (window.count > 0).then(|| window.sum / window.count as f32)
    }
}

fn run(shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) {
    shared.lock().unwrap().status = "starting sensors…".to_string();

    // One-time static device specs (CPU-Z / GPU-Z style), via CPUID + WMI.
    let info = deviceinfo::gather();
    {
        let mut s = shared.lock().unwrap();
        s.device_info = Some(info);
        s.cpu_name = cpu::cpu_brand(); // CPUID — no driver needed
    }

    let gpus = gpu::init();
    log::info!(
        "CPU: {}; GPUs with sensors: {:?}",
        cpu::cpu_brand(),
        gpus.iter().map(|g| g.name()).collect::<Vec<_>>()
    );
    shared.lock().unwrap().gpus = gpus
        .iter()
        .map(|g| GpuState {
            name: g.name().to_string(),
            ..Default::default()
        })
        .collect();
    {
        let mut s = shared.lock().unwrap();
        s.gpu_hists = vec![Default::default(); gpus.len()];
    }

    let client = service::Client::start();
    let mut sysload = sysload::SysLoad::new();
    let mut cpu_window = LoadWindow::default();
    let mut mem_window = LoadWindow::default();
    let mut gpu_windows = vec![LoadWindow::default(); gpus.len()];
    let mut samples_in_point = 0;
    // To log when things change, rather than on every reading.
    let mut service_was_alive = false;
    let mut gpu_was_reading = vec![true; gpus.len()];

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let sample_start = Instant::now();

        let reading = client.reading();
        let service_error = reading.as_ref().and_then(|r| r.error.clone());
        let service_alive = reading.is_some() && service_error.is_none();
        let reading = reading.unwrap_or_default();
        let (cpu_temp, cpu_power, cpu_core_clocks) = (reading.temp, reading.power, reading.cores);
        let failure = SETUP_FAILURE.lock().unwrap().clone().or(service_error).or_else(|| {
            client.other_protocol().then(|| {
                "The sensor service is from another version of Monitor. Click to update it."
                    .to_string()
            })
        });
        let cpu_clock_max = cpu_core_clocks.iter().copied().fold(0.0f32, f32::max);
        let cpu_clock = (cpu_clock_max > 0.0).then_some(cpu_clock_max);

        let (cpu_load, cpu_cores) = match sysload.update() {
            Some((o, c)) => (Some(o), c),
            None => (None, Vec::new()),
        };
        let mem = sysload::memory();
        let gpu_states: Vec<GpuState> = gpus
            .iter()
            .map(|g| {
                let mut state = GpuState {
                    name: g.name().to_string(),
                    load_source: g.load_source(),
                    ..Default::default()
                };
                if let Some(r) = g.read() {
                    state.temp = r.temp;
                    state.clock = r.clock;
                    state.load = r.load;
                    state.power = r.power;
                    state.mem_used_mb = r.mem_used_mb;
                    state.mem_total_mb = r.mem_total_mb;
                }
                state
            })
            .collect();

        if service_alive != service_was_alive {
            service_was_alive = service_alive;
            if service_alive {
                log::info!("sensor service connected");
            } else {
                log::warn!("sensor service gone: {failure:?}");
            }
        }
        for ((g, was_reading), state) in gpus.iter().zip(&mut gpu_was_reading).zip(&gpu_states) {
            let reading = state.temp.is_some() || state.load.is_some() || state.clock.is_some();
            if reading != *was_reading {
                *was_reading = reading;
                if reading {
                    log::info!("{}: readings are back", g.name());
                } else {
                    log::warn!("{}: no readings", g.name());
                }
            }
        }

        cpu_window.add(cpu_load);
        mem_window.add(mem.map(|(pct, _, _)| pct));
        for (window, state) in gpu_windows.iter_mut().zip(&gpu_states) {
            window.add(state.load);
        }
        samples_in_point += 1;
        let add_point = samples_in_point >= SAMPLES_PER_POINT;
        if add_point {
            samples_in_point = 0;
        }

        {
            let mut s = shared.lock().unwrap();
            if add_point {
                s.cpu_hist.push(cpu_window.take());
                s.mem_hist.push(mem_window.take());
                for (hist, window) in s.gpu_hists.iter_mut().zip(gpu_windows.iter_mut()) {
                    hist.push(window.take());
                }
            }
            s.cpu_temp = cpu_temp;
            s.cpu_load = cpu_load;
            s.cpu_clock = cpu_clock;
            s.cpu_power = cpu_power;
            if !cpu_cores.is_empty() {
                s.cpu_cores = cpu_cores;
            }
            s.cpu_core_clocks = cpu_core_clocks; // empty once the service stops
            s.sensor_service_alive = service_alive;
            s.sensor_failure = failure;
            if let Some((pct, used, total)) = mem {
                s.mem_used_pct = Some(pct);
                s.mem_used_gb = used;
                s.mem_total_gb = total;
            }

            let mut sensors = Vec::new();
            if let Some(t) = cpu_temp {
                sensors.push(("CPU · Tctl/Tdie".to_string(), t));
            }
            for g in &gpu_states {
                if let Some(t) = g.temp {
                    sensors.push((g.name.clone(), t));
                }
            }
            s.sensors = sensors;
            s.gpus = gpu_states;

            s.status = if service_alive {
                "live".to_string()
            } else {
                "CPU sensors off — click the TEMP icon".to_string()
            };
        }

        // Sleep the rest of the period, in short steps so that stopping is quick.
        while let Some(left) = SAMPLE_PERIOD.checked_sub(sample_start.elapsed()) {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_window_averages_readings_and_starts_over() {
        let mut w = LoadWindow::default();
        w.add(Some(100.0));
        w.add(Some(0.0));
        assert_eq!(w.take(), Some(50.0));
        assert_eq!(w.take(), None);
    }

    #[test]
    fn load_window_ignores_failed_readings() {
        let mut w = LoadWindow::default();
        w.add(None);
        w.add(Some(40.0));
        assert_eq!(w.take(), Some(40.0));
    }

    #[test]
    fn load_window_with_only_failures_has_no_value() {
        let mut w = LoadWindow::default();
        w.add(None);
        w.add(None);
        assert_eq!(w.take(), None);
    }
}

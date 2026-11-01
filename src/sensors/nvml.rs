// NVIDIA GPU sensors via NVML (nvml.dll, loaded at runtime by nvml-wrapper).
// No admin needed.

use std::rc::Rc;

use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::Nvml;

use super::gpu::{GpuBackend, GpuReading};

pub struct NvmlGpu {
    nvml: Rc<Nvml>,
    index: u32,
    name: String,
}

impl NvmlGpu {
    /// One entry per NVIDIA GPU; empty if NVML isn't available.
    pub fn init_all() -> Vec<NvmlGpu> {
        let Ok(nvml) = Nvml::init() else {
            return Vec::new();
        };
        let nvml = Rc::new(nvml);
        let count = nvml.device_count().unwrap_or(0);
        (0..count)
            .filter_map(|index| {
                let name = nvml.device_by_index(index).ok()?.name().ok()?;
                Some(NvmlGpu {
                    nvml: nvml.clone(),
                    index,
                    name,
                })
            })
            .collect()
    }
}

impl GpuBackend for NvmlGpu {
    fn name(&self) -> &str {
        &self.name
    }

    fn load_source(&self) -> &'static str {
        "NVIDIA driver (NVML): share of recent time the GPU was busy"
    }

    fn read(&self) -> Option<GpuReading> {
        // Device borrows Nvml, so fetch it fresh each read (avoids self-reference).
        let d = self.nvml.device_by_index(self.index).ok()?;
        Some(GpuReading {
            temp: d
                .temperature(TemperatureSensor::Gpu)
                .ok()
                .map(|t| t as f32),
            clock: d.clock_info(Clock::Graphics).ok().map(|c| c as f32),
            load: d.utilization_rates().ok().map(|u| u.gpu as f32),
            // power_usage is milliwatts.
            power: d.power_usage().ok().map(|mw| mw as f32 / 1000.0),
            mem_used_mb: d
                .memory_info()
                .ok()
                .map(|m| m.used as f32 / (1024.0 * 1024.0)),
            mem_total_mb: d
                .memory_info()
                .ok()
                .map(|m| m.total as f32 / (1024.0 * 1024.0)),
        })
    }
}

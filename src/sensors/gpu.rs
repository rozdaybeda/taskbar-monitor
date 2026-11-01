// GPU vendor seam. Each vendor backend implements `GpuBackend`; `init` collects
// every GPU any backend can see — NVIDIA (NVML) first, then AMD (ADL).
// Intel GPUs are not yet supported (would need IGCL / Level Zero Sysman).

use super::adl::AdlGpu;
use super::nvml::NvmlGpu;

pub struct GpuReading {
    pub temp: Option<f32>,
    pub clock: Option<f32>,
    pub load: Option<f32>,
    pub power: Option<f32>,
    pub mem_used_mb: Option<f32>,
    pub mem_total_mb: Option<f32>,
}

pub trait GpuBackend {
    fn name(&self) -> &str;
    /// What this vendor's load figure measures; shown as a tooltip because the
    /// vendors' figures aren't strictly the same metric.
    fn load_source(&self) -> &'static str;
    fn read(&self) -> Option<GpuReading>;
}

/// All GPUs with sensor support, discrete NVIDIA cards first.
pub fn init() -> Vec<Box<dyn GpuBackend>> {
    let mut gpus: Vec<Box<dyn GpuBackend>> = Vec::new();
    for g in NvmlGpu::init_all() {
        gpus.push(Box::new(g));
    }
    for g in AdlGpu::init_all() {
        gpus.push(Box::new(g));
    }
    gpus
}

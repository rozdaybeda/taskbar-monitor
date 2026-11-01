// CPU sensors via PawnIO, for both AMD and Intel.
//   temp  : AMD = SMN thermal register; Intel = Tjmax - IA32_THERM_STATUS readout
//   clock : per-core APERF/MPERF ratio x base freq, max core (vendor-neutral method)
//   load  : GetSystemTimes (vendor-neutral)
//   name  : CPUID brand string
//
// Intel paths are UNVERIFIED on this AMD dev machine — needs Intel hardware.

use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};

use super::pawnio::Driver;

// Signed PawnIO modules, embedded at build time (fetch-pawnio.ps1).
const MODULE_AMD: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/driver/AMDFamily17.bin"));
const MODULE_INTEL: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/driver/IntelMSR.bin"));

// The PawnIO modules allow only the registers they list, and AMD's allows these counters
// through their read-only addresses.
const MSR_AMD_MPERF_RO: u32 = 0xC000_00E7;
const MSR_AMD_APERF_RO: u32 = 0xC000_00E8;
const MSR_INTEL_MPERF: u32 = 0x0000_00E7;
const MSR_INTEL_APERF: u32 = 0x0000_00E8;

// AMD Family 17h/19h
const F17H_THM_TCON_CURRENT_TEMP: u32 = 0x0005_9800;
const F17H_TEMP_OFFSET_FLAG: u32 = 0x0008_0000;
const MSR_AMD_PSTATE_0: u32 = 0xC001_0064;

// Intel
const MSR_INTEL_PLATFORM_INFO: u32 = 0x0000_00CE; // bits 15:8 = max non-turbo ratio
const MSR_INTEL_TEMPERATURE_TARGET: u32 = 0x0000_01A2; // bits 23:16 = Tjmax
const MSR_INTEL_PACKAGE_THERM_STATUS: u32 = 0x0000_01B1; // bits 22:16 = readout below Tjmax

// RAPL energy counters (package power draw)
const MSR_AMD_RAPL_PWR_UNIT: u32 = 0xC001_0299; // bits 12:8 = energy unit
const MSR_AMD_PKG_ENERGY: u32 = 0xC001_029B; // package energy, 32-bit counter
const MSR_INTEL_RAPL_PWR_UNIT: u32 = 0x0000_0606;
const MSR_INTEL_PKG_ENERGY: u32 = 0x0000_0611;

#[derive(Clone, Copy, PartialEq)]
enum Vendor {
    Amd,
    Intel,
    Other,
}

pub struct Cpu {
    drv: Driver,
    vendor: Vendor,
    logical: usize,
    base_mhz: f64,
    tjmax: f32,
    rapl_unit_j: f64,
    prev_energy: Option<(u32, Instant)>,
}

impl Cpu {
    pub fn init() -> Result<Self, String> {
        let vendor = detect_vendor();
        let module = match vendor {
            Vendor::Amd => MODULE_AMD,
            Vendor::Intel => MODULE_INTEL,
            Vendor::Other => return Err("this CPU's sensors aren't supported".to_string()),
        };
        let drv = Driver::open(module)?;
        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        let base_mhz = match vendor {
            Vendor::Amd => amd_p0_mhz(&drv),
            Vendor::Intel => intel_base_mhz(&drv),
            Vendor::Other => 0.0,
        };
        let tjmax = if vendor == Vendor::Intel {
            intel_tjmax(&drv)
        } else {
            0.0
        };
        let rapl_unit_j = rapl_energy_unit(&drv, vendor);

        Ok(Self {
            drv,
            vendor,
            logical,
            base_mhz,
            tjmax,
            rapl_unit_j,
            prev_energy: None,
        })
    }

    pub fn temp(&self) -> Option<f32> {
        match self.vendor {
            Vendor::Amd => {
                let raw = self.drv.read_smn(F17H_THM_TCON_CURRENT_TEMP)?;
                let mut t = ((raw >> 21) & 0x7FF) as f32 * 0.125;
                if raw & F17H_TEMP_OFFSET_FLAG != 0 {
                    t -= 49.0;
                }
                Some(t)
            }
            Vendor::Intel => {
                let status = self.drv.read_msr(MSR_INTEL_PACKAGE_THERM_STATUS)?;
                let readout = ((status >> 16) & 0x7F) as f32;
                Some(self.tjmax - readout)
            }
            Vendor::Other => None,
        }
    }

    /// Per-core effective clock (MHz) over a short window; 0.0 for idle cores.
    /// Vendor-neutral (APERF/MPERF). Empty if base frequency is unknown.
    pub fn core_clocks(&self) -> Vec<f32> {
        if self.base_mhz <= 0.0 {
            return Vec::new();
        }
        let (mperf, aperf) = clock_counters(self.vendor);
        let thread = unsafe { GetCurrentThread() };
        let n = self.logical;
        let mut m0 = vec![0u64; n];
        let mut a0 = vec![0u64; n];
        for i in 0..n {
            unsafe { SetThreadAffinityMask(thread, 1usize << i) };
            m0[i] = self.drv.read_msr(mperf).unwrap_or(0);
            a0[i] = self.drv.read_msr(aperf).unwrap_or(0);
        }
        sleep(Duration::from_millis(120));
        let mut out = vec![0.0f32; n];
        for i in 0..n {
            unsafe { SetThreadAffinityMask(thread, 1usize << i) };
            let m1 = self.drv.read_msr(mperf).unwrap_or(0);
            let a1 = self.drv.read_msr(aperf).unwrap_or(0);
            let md = m1.wrapping_sub(m0[i]);
            let ad = a1.wrapping_sub(a0[i]);
            if md > 0 {
                out[i] = ((ad as f64 / md as f64) * self.base_mhz) as f32;
            }
        }
        let all = if n >= usize::BITS as usize {
            usize::MAX
        } else {
            (1usize << n) - 1
        };
        unsafe { SetThreadAffinityMask(thread, all) };
        out
    }

    /// Package power draw (watts) from the RAPL energy counter, measured as the
    /// energy delta since the previous call. `None` on the first call.
    pub fn power(&mut self) -> Option<f32> {
        if self.rapl_unit_j <= 0.0 {
            return None;
        }
        let reg = match self.vendor {
            Vendor::Amd => MSR_AMD_PKG_ENERGY,
            Vendor::Intel => MSR_INTEL_PKG_ENERGY,
            Vendor::Other => return None,
        };
        let raw = (self.drv.read_msr(reg)? & 0xFFFF_FFFF) as u32;
        let now = Instant::now();
        let result = if let Some((prev_raw, prev_t)) = self.prev_energy {
            let dt = now.duration_since(prev_t).as_secs_f64();
            let d_raw = raw.wrapping_sub(prev_raw) as f64;
            if dt > 0.0 {
                Some((d_raw * self.rapl_unit_j / dt) as f32)
            } else {
                None
            }
        } else {
            None
        };
        self.prev_energy = Some((raw, now));
        result
    }
}

/// The (MPERF, APERF) registers for this CPU.
fn clock_counters(vendor: Vendor) -> (u32, u32) {
    match vendor {
        Vendor::Amd => (MSR_AMD_MPERF_RO, MSR_AMD_APERF_RO),
        _ => (MSR_INTEL_MPERF, MSR_INTEL_APERF),
    }
}

/// RAPL energy unit in joules per counter tick (0.5^ESU).
fn rapl_energy_unit(drv: &Driver, vendor: Vendor) -> f64 {
    let reg = match vendor {
        Vendor::Amd => MSR_AMD_RAPL_PWR_UNIT,
        Vendor::Intel => MSR_INTEL_RAPL_PWR_UNIT,
        Vendor::Other => return 0.0,
    };
    match drv.read_msr(reg) {
        Some(v) => {
            let esu = ((v >> 8) & 0x1F) as i32;
            0.5f64.powi(esu)
        }
        None => 0.0,
    }
}

fn amd_p0_mhz(drv: &Driver) -> f64 {
    match drv.read_msr(MSR_AMD_PSTATE_0) {
        Some(v) => {
            let fid = (v & 0xFF) as f64;
            let did = ((v >> 8) & 0x3F) as f64;
            if did > 0.0 {
                fid / did * 200.0
            } else {
                0.0
            }
        }
        None => 0.0,
    }
}

/// Intel base (max non-turbo) frequency: PLATFORM_INFO bits 15:8 x 100 MHz.
fn intel_base_mhz(drv: &Driver) -> f64 {
    match drv.read_msr(MSR_INTEL_PLATFORM_INFO) {
        Some(v) => ((v >> 8) & 0xFF) as f64 * 100.0,
        None => 0.0,
    }
}

/// Intel Tjmax: TEMPERATURE_TARGET bits 23:16 (default 100 C if unavailable).
fn intel_tjmax(drv: &Driver) -> f32 {
    match drv.read_msr(MSR_INTEL_TEMPERATURE_TARGET) {
        Some(v) => {
            let t = ((v >> 16) & 0xFF) as f32;
            if t > 0.0 {
                t
            } else {
                100.0
            }
        }
        None => 100.0,
    }
}

fn detect_vendor() -> Vendor {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        let r = __cpuid(0);
        let mut s = [0u8; 12];
        s[0..4].copy_from_slice(&r.ebx.to_le_bytes());
        s[4..8].copy_from_slice(&r.edx.to_le_bytes());
        s[8..12].copy_from_slice(&r.ecx.to_le_bytes());
        match &s {
            b"AuthenticAMD" => Vendor::Amd,
            b"GenuineIntel" => Vendor::Intel,
            _ => Vendor::Other,
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        Vendor::Other
    }
}

/// CPU brand string from CPUID leaves 0x80000002..0x80000004. No driver needed.
pub(crate) fn cpu_brand() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        if __cpuid(0x8000_0000).eax < 0x8000_0004 {
            return "Unknown CPU".to_string();
        }
        let mut bytes = Vec::with_capacity(48);
        for leaf in [0x8000_0002u32, 0x8000_0003, 0x8000_0004] {
            let r = __cpuid(leaf);
            for reg in [r.eax, r.ebx, r.ecx, r.edx] {
                bytes.extend_from_slice(&reg.to_le_bytes());
            }
        }
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).trim().to_string()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "Unknown CPU".to_string()
    }
}

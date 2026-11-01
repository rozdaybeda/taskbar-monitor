// OS-level, vendor-neutral readings: per-core CPU load (NtQuerySystemInformation)
// and physical memory usage (GlobalMemoryStatusEx). No driver, no admin.

use std::ffi::c_void;

use windows::core::{s, w};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcPerf {
    idle: i64,
    kernel: i64, // includes idle
    user: i64,
    dpc: i64,
    interrupt: i64,
    interrupt_count: u32,
}

type NtQsi = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;

pub struct SysLoad {
    ntqsi: Option<NtQsi>,
    n: usize,
    prev_idle: Vec<i64>,
    prev_total: Vec<i64>,
}

impl SysLoad {
    pub fn new() -> Self {
        let n = std::thread::available_parallelism()
            .map(|x| x.get())
            .unwrap_or(1);
        let ntqsi = unsafe {
            LoadLibraryW(w!("ntdll.dll")).ok().and_then(|h| {
                GetProcAddress(h, s!("NtQuerySystemInformation"))
                    .map(|p| std::mem::transmute::<_, NtQsi>(p))
            })
        };
        Self {
            ntqsi,
            n,
            prev_idle: Vec::new(),
            prev_total: Vec::new(),
        }
    }

    /// Returns (overall load %, per-core load %). `None` on the first call
    /// (needs a previous sample) or if the query fails.
    pub fn update(&mut self) -> Option<(f32, Vec<f32>)> {
        let f = self.ntqsi?;
        let mut buf = vec![ProcPerf::default(); self.n];
        let len = (self.n * std::mem::size_of::<ProcPerf>()) as u32;
        let mut ret = 0u32;
        let status = unsafe {
            f(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                buf.as_mut_ptr() as *mut c_void,
                len,
                &mut ret,
            )
        };
        if status != 0 {
            return None;
        }
        let count = ((ret as usize) / std::mem::size_of::<ProcPerf>()).min(self.n);

        let idle: Vec<i64> = (0..count).map(|i| buf[i].idle).collect();
        let total: Vec<i64> = (0..count).map(|i| buf[i].kernel + buf[i].user).collect();

        if self.prev_idle.len() != count {
            self.prev_idle = idle;
            self.prev_total = total;
            return None;
        }

        let mut cores = Vec::with_capacity(count);
        let mut sum_idle = 0i64;
        let mut sum_total = 0i64;
        for i in 0..count {
            let di = idle[i] - self.prev_idle[i];
            let dt = total[i] - self.prev_total[i];
            sum_idle += di;
            sum_total += dt;
            let load = if dt > 0 {
                (1.0 - di as f64 / dt as f64) * 100.0
            } else {
                0.0
            };
            cores.push(load.clamp(0.0, 100.0) as f32);
        }
        self.prev_idle = idle;
        self.prev_total = total;

        let overall = if sum_total > 0 {
            ((1.0 - sum_idle as f64 / sum_total as f64) * 100.0) as f32
        } else {
            0.0
        };
        Some((overall, cores))
    }
}

/// Physical memory: (used %, used GB, total GB).
pub fn memory() -> Option<(f32, f32, f32)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe {
        GlobalMemoryStatusEx(&mut m).ok()?;
    }
    let gb = 1024.0 * 1024.0 * 1024.0;
    let total = m.ullTotalPhys as f32 / gb;
    let avail = m.ullAvailPhys as f32 / gb;
    Some((m.dwMemoryLoad as f32, total - avail, total))
}

// AMD GPU sensors via ADL (atiadlxx.dll), dynamically loaded. No admin needed.
// Reads Overdrive8 PMLog data: clock (id 1), load (id 19), temperature
// (first supported SoC/VR sensor). Struct layouts from LibreHardwareMonitor.

use std::collections::HashSet;
use std::ffi::c_void;
use std::rc::Rc;

use windows::core::{s, w, PCSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{GetProcessHeap, HeapAlloc, HEAP_ZERO_MEMORY};

use super::gpu::{GpuBackend, GpuReading};

const ADL_OK: i32 = 0;
const ATI_VENDOR_ID: i32 = 1002; // ADL reports vendor id in decimal
const ADL_PMLOG_MAX_SENSORS: usize = 256;
const PMLOG_CLK_GFXCLK: usize = 1;
const PMLOG_INFO_ACTIVITY_GFX: usize = 19;
// Temperature candidates (edge/8 is unsupported on iGPUs; SoC/VR sensors carry it).
const TEMP_CANDIDATES: [usize; 5] = [32, 29, 28, 20, 8];
// Board/ASIC power (W): id 73 on RDNA3+, falling back to older SoC/GFX power ids.
const POWER_CANDIDATES: [usize; 3] = [73, 23, 17];

#[repr(C)]
#[derive(Clone, Copy)]
struct AdapterInfo {
    size: i32,
    adapter_index: i32,
    udid: [u8; 256],
    bus_number: i32,
    device_number: i32,
    function_number: i32,
    vendor_id: i32,
    adapter_name: [u8; 256],
    display_name: [u8; 256],
    present: i32,
    exist: i32,
    driver_path: [u8; 256],
    driver_path_ext: [u8; 256],
    pnp_string: [u8; 256],
    os_display_index: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SingleSensorData {
    supported: i32,
    value: i32,
}

#[repr(C)]
struct PmLogDataOutput {
    size: i32,
    sensors: [SingleSensorData; ADL_PMLOG_MAX_SENSORS],
}

type MallocCb = unsafe extern "C" fn(i32) -> *mut c_void;
type FnCreate = unsafe extern "C" fn(MallocCb, i32, *mut *mut c_void) -> i32;
type FnDestroy = unsafe extern "C" fn(*mut c_void) -> i32;
type FnNumAdapters = unsafe extern "C" fn(*mut c_void, *mut i32) -> i32;
type FnAdapterInfo = unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> i32;
type FnQueryPmLog = unsafe extern "C" fn(*mut c_void, i32, *mut PmLogDataOutput) -> i32;
type FnVramUsage = unsafe extern "C" fn(*mut c_void, i32, *mut i32) -> i32;
type FnMemInfo = unsafe extern "C" fn(*mut c_void, i32, *mut AdlMemoryInfo) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct AdlMemoryInfo {
    mem_size: i64, // bytes
    mem_type: [u8; 256],
    mem_bandwidth: i64,
}

unsafe extern "C" fn adl_malloc(size: i32) -> *mut c_void {
    if size <= 0 {
        return std::ptr::null_mut();
    }
    match GetProcessHeap() {
        Ok(h) => HeapAlloc(h, HEAP_ZERO_MEMORY, size as usize),
        Err(_) => std::ptr::null_mut(),
    }
}

unsafe fn proc<T>(h: HMODULE, name: PCSTR) -> Option<T> {
    let p = GetProcAddress(h, name)?;
    Some(std::mem::transmute_copy::<_, T>(&p))
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// The loaded DLL and its ADL context, shared by every AMD adapter.
struct AdlLib {
    dll: HMODULE,
    ctx: *mut c_void,
    destroy: FnDestroy,
    pm_get: FnQueryPmLog,
    vram_usage: Option<FnVramUsage>,
}

impl Drop for AdlLib {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.ctx);
            let _ = FreeLibrary(self.dll);
        }
    }
}

pub struct AdlGpu {
    lib: Rc<AdlLib>,
    mem_total_mb: Option<f32>,
    adapter: i32,
    pub name: String,
}

impl AdlGpu {
    /// One entry per physical AMD GPU; empty if ADL isn't available.
    pub fn init_all() -> Vec<AdlGpu> {
        Self::try_init_all().unwrap_or_default()
    }

    fn try_init_all() -> Option<Vec<AdlGpu>> {
        unsafe {
            let dll = LoadLibraryW(w!("atiadlxx.dll")).ok()?;
            let create: FnCreate = proc(dll, s!("ADL2_Main_Control_Create"))?;
            let destroy: FnDestroy = proc(dll, s!("ADL2_Main_Control_Destroy"))?;
            let num_get: FnNumAdapters = proc(dll, s!("ADL2_Adapter_NumberOfAdapters_Get"))?;
            let info_get: FnAdapterInfo = proc(dll, s!("ADL2_Adapter_AdapterInfo_Get"))?;
            let pm_get: FnQueryPmLog = proc(dll, s!("ADL2_New_QueryPMLogData_Get"))?;

            let mut ctx: *mut c_void = std::ptr::null_mut();
            if create(adl_malloc, 1, &mut ctx) != ADL_OK || ctx.is_null() {
                let _ = FreeLibrary(dll);
                return None;
            }

            // Optional VRAM capabilities (may be absent on older drivers).
            let lib = Rc::new(AdlLib {
                dll,
                ctx,
                destroy,
                pm_get,
                vram_usage: proc(dll, s!("ADL2_Adapter_DedicatedVRAMUsage_Get")),
            });
            let mem_info: Option<FnMemInfo> = proc(dll, s!("ADL2_Adapter_MemoryInfo_Get"));

            let mut n = 0i32;
            if num_get(ctx, &mut n) != ADL_OK || n <= 0 {
                return None;
            }

            let count = n as usize;
            let mut infos: Vec<AdapterInfo> = vec![std::mem::zeroed(); count];
            let bytes = (count * std::mem::size_of::<AdapterInfo>()) as i32;
            if info_get(ctx, infos.as_mut_ptr() as *mut c_void, bytes) != ADL_OK {
                return None;
            }

            // ADL lists one adapter per display output, so a GPU can appear many
            // times; keep the first entry for each PCI address.
            let mut seen = HashSet::new();
            let gpus = infos
                .iter()
                .filter(|a| a.vendor_id == ATI_VENDOR_ID && a.exist != 0)
                .filter(|a| seen.insert((a.bus_number, a.device_number, a.function_number)))
                .map(|a| {
                    let adapter = a.adapter_index;
                    let mem_total_mb = mem_info.and_then(|f| {
                        let mut mi: AdlMemoryInfo = std::mem::zeroed();
                        if f(ctx, adapter, &mut mi) == ADL_OK && mi.mem_size > 0 {
                            Some(mi.mem_size as f32 / (1024.0 * 1024.0))
                        } else {
                            None
                        }
                    });
                    AdlGpu {
                        lib: lib.clone(),
                        mem_total_mb,
                        adapter,
                        name: cstr(&a.adapter_name),
                    }
                })
                .collect();
            Some(gpus)
        }
    }
}

impl GpuBackend for AdlGpu {
    fn name(&self) -> &str {
        &self.name
    }

    fn load_source(&self) -> &'static str {
        "AMD driver (ADL): graphics activity reading"
    }

    fn read(&self) -> Option<GpuReading> {
        unsafe {
            let mut out: PmLogDataOutput = std::mem::zeroed();
            if (self.lib.pm_get)(self.lib.ctx, self.adapter, &mut out) != ADL_OK {
                return None;
            }
            let get = |i: usize| -> Option<f32> {
                let s = out.sensors[i];
                if s.supported != 0 {
                    Some(s.value as f32)
                } else {
                    None
                }
            };
            let temp = TEMP_CANDIDATES.iter().find_map(|&i| get(i));
            let power = POWER_CANDIDATES.iter().find_map(|&i| get(i));
            let mem_used_mb = self.lib.vram_usage.and_then(|f| {
                let mut mb = 0i32;
                if f(self.lib.ctx, self.adapter, &mut mb) == ADL_OK {
                    Some(mb as f32)
                } else {
                    None
                }
            });
            Some(GpuReading {
                temp,
                clock: get(PMLOG_CLK_GFXCLK),
                load: get(PMLOG_INFO_ACTIVITY_GFX),
                power,
                mem_used_mb,
                mem_total_mb: self.mem_total_mb,
            })
        }
    }
}

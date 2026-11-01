// Static device specifications (CPU-Z / GPU-Z style), gathered once at startup
// via CPUID + WMI. WMI is Windows' native COM API (not .NET), used through the
// `wmi` crate. Each section is a list of (label, value) pairs for easy display.

use serde::Deserialize;
use wmi::{COMLibrary, WMIConnection};

#[derive(Clone, Default)]
pub struct DeviceInfo {
    pub cpu: Vec<(String, String)>,
    pub cpu_isa: Vec<(String, String)>,
    /// One (name, rows) entry per physical GPU.
    pub gpus: Vec<(String, Vec<(String, String)>)>,
    pub mem_summary: Vec<(String, String)>,
    pub mem_modules: Vec<(String, Vec<(String, String)>)>,
    pub mainboard: Vec<(String, String)>,
}

pub fn gather() -> DeviceInfo {
    let (cpu_rows, cpu_isa) = cpuid_cpu();
    let mut di = DeviceInfo {
        cpu: cpu_rows,
        cpu_isa,
        ..Default::default()
    };

    let wmi = COMLibrary::new().and_then(WMIConnection::new);
    match wmi {
        Ok(wmi) => {
            augment_cpu(&wmi, &mut di.cpu);
            di.gpus = gpu_info(&wmi);
            let (summary, modules) = mem_info(&wmi);
            di.mem_summary = summary;
            di.mem_modules = modules;
            di.mainboard = mainboard_info(&wmi);
        }
        Err(e) => log::warn!("WMI unavailable, device info will be limited: {e}"),
    }
    di
}

// ---------------------------------------------------------------------------
// CPU — CPUID
// ---------------------------------------------------------------------------

#[allow(unused_unsafe)]
fn cpuid_cpu() -> (Vec<(String, String)>, Vec<(String, String)>) {
    let mut v = Vec::new();
    let mut isa: Vec<(String, String)> = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{__cpuid, __cpuid_count};
        unsafe {
            let r0 = __cpuid(0);
            let mut vend = [0u8; 12];
            vend[0..4].copy_from_slice(&r0.ebx.to_le_bytes());
            vend[4..8].copy_from_slice(&r0.edx.to_le_bytes());
            vend[8..12].copy_from_slice(&r0.ecx.to_le_bytes());
            let vendor = String::from_utf8_lossy(&vend).trim().to_string();

            let r1 = __cpuid(1);
            let eax = r1.eax;
            let stepping = eax & 0xF;
            let base_model = (eax >> 4) & 0xF;
            let base_family = (eax >> 8) & 0xF;
            let ext_model = (eax >> 16) & 0xF;
            let ext_family = (eax >> 20) & 0xFF;
            let family = if base_family == 0xF {
                base_family + ext_family
            } else {
                base_family
            };
            let model = if base_family == 0x6 || base_family == 0xF {
                (ext_model << 4) | base_model
            } else {
                base_model
            };

            let r7 = __cpuid_count(7, 0);
            let bit = |x: u32, b: u32| x & (1 << b) != 0;
            // Grouped as label + supported features; empty groups are left out.
            let mut group = |label: &str, features: &[(bool, &str)]| {
                let names: Vec<&str> = features
                    .iter()
                    .filter(|(supported, _)| *supported)
                    .map(|(_, name)| *name)
                    .collect();
                if !names.is_empty() {
                    isa.push((label.to_string(), names.join("   ")));
                }
            };
            group(
                "SSE",
                &[
                    (bit(r1.edx, 25), "SSE"),
                    (bit(r1.edx, 26), "SSE2"),
                    (bit(r1.ecx, 0), "SSE3"),
                    (bit(r1.ecx, 9), "SSSE3"),
                    (bit(r1.ecx, 19), "SSE4.1"),
                    (bit(r1.ecx, 20), "SSE4.2"),
                ],
            );
            group(
                "AVX",
                &[
                    (bit(r1.ecx, 28), "AVX"),
                    (bit(r7.ebx, 5), "AVX2"),
                    (bit(r7.ebx, 16), "AVX-512F"),
                    (bit(r1.ecx, 12), "FMA"),
                ],
            );
            group(
                "Crypto",
                &[(bit(r1.ecx, 25), "AES"), (bit(r7.ebx, 29), "SHA")],
            );
            group(
                "Other",
                &[
                    (bit(r1.edx, 23), "MMX"),
                    (bit(r7.ebx, 3), "BMI1"),
                    (bit(r7.ebx, 8), "BMI2"),
                ],
            );

            v.push(("Name".into(), brand_string()));
            v.push(("Vendor".into(), vendor));
            v.push((
                "Family / Model / Step".into(),
                format!("{family:X}h / {model:X}h / {stepping}"),
            ));
        }
    }
    (v, isa)
}

#[cfg(target_arch = "x86_64")]
#[allow(unused_unsafe)]
fn brand_string() -> String {
    use std::arch::x86_64::__cpuid;
    unsafe {
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
}

#[cfg(not(target_arch = "x86_64"))]
fn brand_string() -> String {
    "Unknown CPU".to_string()
}

// ---------------------------------------------------------------------------
// WMI structs (field names match WMI property names exactly)
// ---------------------------------------------------------------------------

#[allow(non_snake_case, non_camel_case_types)]
#[derive(Deserialize)]
struct Win32_Processor {
    NumberOfCores: Option<u32>,
    NumberOfLogicalProcessors: Option<u32>,
    MaxClockSpeed: Option<u32>,
    SocketDesignation: Option<String>,
    L2CacheSize: Option<u32>,
    L3CacheSize: Option<u32>,
}

#[allow(non_snake_case, non_camel_case_types)]
#[derive(Deserialize)]
struct Win32_PhysicalMemory {
    DeviceLocator: Option<String>,
    BankLabel: Option<String>,
    Manufacturer: Option<String>,
    PartNumber: Option<String>,
    Capacity: Option<u64>,
    Speed: Option<u32>,
    ConfiguredClockSpeed: Option<u32>,
    SMBIOSMemoryType: Option<u32>,
}

#[allow(non_snake_case, non_camel_case_types)]
#[derive(Deserialize)]
struct Win32_PhysicalMemoryArray {
    MemoryDevices: Option<u32>,
}

#[allow(non_snake_case, non_camel_case_types)]
#[derive(Deserialize)]
struct Win32_VideoController {
    Name: Option<String>,
    DriverVersion: Option<String>,
    AdapterRAM: Option<u32>,
    VideoProcessor: Option<String>,
    PNPDeviceID: Option<String>,
}

fn augment_cpu(wmi: &WMIConnection, cpu: &mut Vec<(String, String)>) {
    let procs: Vec<Win32_Processor> = wmi.query().unwrap_or_default();
    if let Some(p) = procs.into_iter().next() {
        if let (Some(c), Some(l)) = (p.NumberOfCores, p.NumberOfLogicalProcessors) {
            cpu.push(("Cores / Threads".into(), format!("{c} / {l}")));
        }
        if let Some(s) = p.SocketDesignation {
            cpu.push(("Socket".into(), s));
        }
        if let Some(m) = p.MaxClockSpeed {
            cpu.push(("Max Clock".into(), format!("{m} MHz")));
        }
        let l2 = p.L2CacheSize.map(|k| format!("L2 {} MB", k / 1024));
        let l3 = p.L3CacheSize.map(|k| format!("L3 {} MB", k / 1024));
        let cache: Vec<String> = [l2, l3].into_iter().flatten().collect();
        if !cache.is_empty() {
            cpu.push(("Cache".into(), cache.join("  ·  ")));
        }
    }
}

fn gpu_info(wmi: &WMIConnection) -> Vec<(String, Vec<(String, String)>)> {
    let all: Vec<Win32_VideoController> = wmi.query().unwrap_or_default();
    // Real GPUs sit on PCI; this skips remote-desktop and other virtual adapters.
    let is_pci = |g: &Win32_VideoController| {
        g.PNPDeviceID
            .as_deref()
            .is_some_and(|id| id.starts_with("PCI\\"))
    };
    let any_pci = all.iter().any(is_pci);
    let vram = dxgi_vram_mb();

    all.iter()
        .filter(|g| !any_pci || is_pci(g))
        .map(|g| {
            let ids = g.PNPDeviceID.as_deref().and_then(parse_ven_dev);
            let mut rows = Vec::new();
            if let Some(v) = &g.VideoProcessor {
                rows.push(("Processor".to_string(), v.clone()));
            }
            // WMI's AdapterRAM is 32-bit and tops out at 4 GB, so prefer DXGI.
            let dxgi_mem = ids.as_ref().and_then(|(ven, dev)| {
                let key = (u32::from_str_radix(ven, 16).ok()?, u32::from_str_radix(dev, 16).ok()?);
                vram.iter().find(|(k, _)| *k == key).map(|(_, mb)| *mb)
            });
            let wmi_mem = g.AdapterRAM.map(|r| (r / (1024 * 1024)) as u64);
            if let Some(mb) = dxgi_mem.or(wmi_mem) {
                rows.push(("Memory".to_string(), format!("{mb} MB")));
            }
            if let Some(d) = &g.DriverVersion {
                rows.push(("Driver".to_string(), d.clone()));
            }
            if let Some((ven, dev)) = ids {
                rows.push(("Vendor / Device".to_string(), format!("{ven} / {dev}")));
            }
            (g.Name.clone().unwrap_or_else(|| "GPU".to_string()), rows)
        })
        .collect()
}

/// Dedicated video memory (MB) per adapter, keyed by PCI (vendor, device) id.
fn dxgi_vram_mb() -> Vec<((u32, u32), u64)> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            if let Ok(desc) = adapter.GetDesc1() {
                out.push((
                    (desc.VendorId, desc.DeviceId),
                    desc.DedicatedVideoMemory as u64 / (1024 * 1024),
                ));
            }
            i += 1;
        }
    }
    out
}

fn parse_ven_dev(pnp: &str) -> Option<(String, String)> {
    let ven = pnp.find("VEN_").map(|i| pnp[i + 4..i + 8].to_string());
    let dev = pnp.find("DEV_").map(|i| pnp[i + 4..i + 8].to_string());
    match (ven, dev) {
        (Some(v), Some(d)) => Some((v, d)),
        _ => None,
    }
}

#[allow(clippy::type_complexity)]
fn mem_info(
    wmi: &WMIConnection,
) -> (Vec<(String, String)>, Vec<(String, Vec<(String, String)>)>) {
    let mut summary = Vec::new();
    let arrays: Vec<Win32_PhysicalMemoryArray> = wmi.query().unwrap_or_default();
    let slots_total: u32 = arrays.iter().filter_map(|a| a.MemoryDevices).sum();

    let modules: Vec<Win32_PhysicalMemory> = wmi.query().unwrap_or_default();
    let total_gb: f64 = modules
        .iter()
        .filter_map(|m| m.Capacity)
        .map(|c| c as f64 / (1024.0 * 1024.0 * 1024.0))
        .sum();

    summary.push(("Total".into(), format!("{total_gb:.1} GB")));
    if slots_total > 0 {
        summary.push((
            "Slots".into(),
            format!("{} used / {} total", modules.len(), slots_total),
        ));
    } else {
        summary.push(("Modules".into(), modules.len().to_string()));
    }

    let module_rows = modules
        .into_iter()
        .map(|m| {
            // DeviceLocator is often identical across slots (e.g. "DIMM 0"); the
            // BankLabel (e.g. "P0 CHANNEL A") distinguishes them, so fold its last
            // token into the title.
            let loc = non_empty(m.DeviceLocator).unwrap_or_else(|| "DIMM".into());
            let title = match non_empty(m.BankLabel) {
                Some(bank) => {
                    let ch = bank.rsplit(' ').next().unwrap_or(bank.as_str());
                    format!("{loc} · {ch}")
                }
                None => loc,
            };
            let mut rows = Vec::new();
            if let Some(c) = m.Capacity {
                rows.push((
                    "Size".into(),
                    format!("{:.0} GB", c as f64 / (1024.0 * 1024.0 * 1024.0)),
                ));
            }
            if let Some(t) = m.SMBIOSMemoryType {
                rows.push(("Type".into(), ddr_type(t).to_string()));
            }
            if let Some(s) = m.ConfiguredClockSpeed.or(m.Speed) {
                rows.push(("Speed".into(), format!("{s} MHz")));
            }
            if let Some(v) = non_empty(m.Manufacturer) {
                rows.push(("Vendor".into(), v));
            }
            if let Some(p) = non_empty(m.PartNumber) {
                rows.push(("Part".into(), p));
            }
            (title, rows)
        })
        .collect();

    (summary, module_rows)
}

#[allow(non_snake_case, non_camel_case_types)]
#[derive(Deserialize)]
struct Win32_BaseBoard {
    Manufacturer: Option<String>,
    Product: Option<String>,
    Version: Option<String>,
}

#[allow(non_snake_case, non_camel_case_types)]
#[derive(Deserialize)]
struct Win32_BIOS {
    Manufacturer: Option<String>,
    SMBIOSBIOSVersion: Option<String>,
}

#[allow(non_snake_case, non_camel_case_types)]
#[derive(Deserialize)]
struct Win32_ComputerSystem {
    Manufacturer: Option<String>,
    Model: Option<String>,
}

fn mainboard_info(wmi: &WMIConnection) -> Vec<(String, String)> {
    let mut out = Vec::new();

    let systems: Vec<Win32_ComputerSystem> = wmi.query().unwrap_or_default();
    if let Some(s) = systems.into_iter().next() {
        if let Some(m) = non_empty(s.Manufacturer) {
            out.push(("System".into(), m));
        }
        if let Some(m) = non_empty(s.Model) {
            out.push(("Model".into(), m));
        }
    }

    let boards: Vec<Win32_BaseBoard> = wmi.query().unwrap_or_default();
    if let Some(b) = boards.into_iter().next() {
        if let Some(m) = non_empty(b.Manufacturer) {
            out.push(("Board Vendor".into(), m));
        }
        if let Some(p) = non_empty(b.Product) {
            out.push(("Board Model".into(), p));
        }
        if let Some(v) = non_empty(b.Version) {
            out.push(("Board Rev".into(), v));
        }
    }

    let bios: Vec<Win32_BIOS> = wmi.query().unwrap_or_default();
    if let Some(bi) = bios.into_iter().next() {
        if let Some(m) = non_empty(bi.Manufacturer) {
            out.push(("BIOS Vendor".into(), m));
        }
        if let Some(v) = non_empty(bi.SMBIOSBIOSVersion) {
            out.push(("BIOS Version".into(), v));
        }
    }

    out
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
}

fn ddr_type(t: u32) -> &'static str {
    match t {
        20 => "DDR",
        21 => "DDR2",
        24 => "DDR3",
        26 => "DDR4",
        34 => "DDR5",
        _ => "RAM",
    }
}

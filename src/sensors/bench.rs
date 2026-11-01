// On-demand speed tests for the CPU, system RAM and each GPU. All values are
// decimal (1 GB/s = 10^9 bytes per second).
//
// CPU  : cache bandwidth and latency on one thread, using working sets that fit
//        each cache level, plus integer and AVX2 floating-point throughput on one
//        thread and on all of them.
// RAM  : every kernel runs on all logical CPUs at once.
//        CPU and RAM kernels are inline assembly or vcruntime's memset/memcpy, so
//        debug and release builds give the same numbers.
// GPU  : Direct3D 11 compute, so one code path covers every vendor, with no
//        driver SDK. Shaders are compiled at run time by d3dcompiler_47.dll, which
//        ships with Windows.
//
// Latency uses a pointer chase through one random cycle, so the prefetchers can't
// help. It runs on normal 4 KB pages (RAM) and driver-chosen pages (GPU), so it
// includes TLB misses and reads higher than figures measured with large pages.
//
// Stress tests hold a device at full load until stopped, while recording its
// temperature, clock and power. They also recompute a known result over and over,
// so an unstable CPU or GPU shows up as calculation errors, and they stop early if
// a safety temperature is reached.
//
// Each test checks its own work where it can (the chase must land on the expected
// node, downloaded data must match what was uploaded) and fails rather than
// report a number from a run the driver skipped or optimized away.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::arch::asm;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use windows::core::{s, PCSTR};
use windows::Win32::Foundation::{BOOL, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

use super::{cpu, sysload};
use crate::Shared;

const GROUP_BANDWIDTH: &str = "BANDWIDTH";
const GROUP_LATENCY: &str = "LATENCY";
const GROUP_THROUGHPUT: &str = "THROUGHPUT";
const GROUP_STRESS: &str = "STRESS TEST";

#[derive(Clone)]
pub struct BenchRow {
    pub group: &'static str,
    pub label: &'static str,
    pub value: String,
}

#[derive(Clone, PartialEq)]
pub enum BenchStatus {
    Idle,
    Running(&'static str),
    Stressing,
    Done,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq)]
pub enum BenchKind {
    Cpu,
    Memory,
    Gpu,
}

#[derive(Clone, Copy)]
enum Target {
    Cpu,
    Memory,
    Gpu(LUID),
}

/// One testable device and its latest results.
#[derive(Clone)]
pub struct BenchSlot {
    pub name: String,
    pub kind: BenchKind,
    target: Target,
    pub status: BenchStatus,
    pub rows: Vec<BenchRow>,
    stop: Arc<AtomicBool>,
}

/// Runs one test at a time on a background thread; the UI polls `snapshot`.
pub struct BenchRunner {
    slots: Arc<Mutex<Vec<BenchSlot>>>,
    shared: Arc<Mutex<Shared>>,
}

impl BenchRunner {
    pub fn new(shared: Arc<Mutex<Shared>>) -> Self {
        let mut slots = vec![
            new_slot(cpu::cpu_brand(), Target::Cpu),
            new_slot("System memory".to_string(), Target::Memory),
        ];
        for (name, luid) in gpu_adapters() {
            slots.push(new_slot(name, Target::Gpu(luid)));
        }
        Self {
            slots: Arc::new(Mutex::new(slots)),
            shared,
        }
    }

    pub fn snapshot(&self) -> Vec<BenchSlot> {
        self.slots.lock().unwrap().clone()
    }

    /// Start the test in slot `index`, unless a test is already running.
    pub fn start(&self, index: usize) {
        let target = {
            let mut slots = self.slots.lock().unwrap();
            // Speed results would be meaningless with anything else loading the machine.
            let busy = slots
                .iter()
                .any(|s| matches!(s.status, BenchStatus::Running(_) | BenchStatus::Stressing));
            if busy {
                return;
            }
            let Some(slot) = slots.get_mut(index) else {
                return;
            };
            slot.status = BenchStatus::Running("Preparing");
            slot.rows.clear();
            log::info!("speed test started: {}", slot.name);
            slot.target
        };

        let reporter = Reporter {
            slots: self.slots.clone(),
            index,
        };
        std::thread::spawn(move || {
            let result = match target {
                Target::Cpu => run_cpu(&reporter),
                Target::Memory => run_memory(&reporter),
                Target::Gpu(luid) => run_gpu(&reporter, luid),
            };
            reporter.finish("speed test", result);
        });
    }

    /// Start a stress test on slot `index`. Several devices can be stressed at
    /// once, for a whole-system load, but not during a speed test.
    pub fn start_stress(&self, index: usize) {
        let (target, stop) = {
            let mut slots = self.slots.lock().unwrap();
            let speed_test_running = slots
                .iter()
                .any(|s| matches!(s.status, BenchStatus::Running(_)));
            if speed_test_running {
                return;
            }
            let Some(slot) = slots.get_mut(index) else {
                return;
            };
            if slot.status == BenchStatus::Stressing || slot.kind == BenchKind::Memory {
                return;
            }
            slot.status = BenchStatus::Stressing;
            log::info!("stress test started: {}", slot.name);
            slot.rows.retain(|r| r.group != GROUP_STRESS);
            slot.stop.store(false, Ordering::SeqCst);
            (slot.target, slot.stop.clone())
        };

        let reporter = Reporter {
            slots: self.slots.clone(),
            index,
        };
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let result = match target {
                Target::Cpu => stress_cpu(&reporter, &shared, &stop),
                Target::Gpu(luid) => {
                    let name = reporter.slot_name();
                    stress_gpu(&reporter, &shared, &stop, luid, &name)
                }
                Target::Memory => Ok(()),
            };
            reporter.finish("stress test", result);
        });
    }

    pub fn stop_stress(&self, index: usize) {
        if let Some(slot) = self.slots.lock().unwrap().get(index) {
            slot.stop.store(true, Ordering::SeqCst);
        }
    }

    pub fn stop_all_stress(&self) {
        for slot in self.slots.lock().unwrap().iter() {
            slot.stop.store(true, Ordering::SeqCst);
        }
    }
}

fn new_slot(name: String, target: Target) -> BenchSlot {
    BenchSlot {
        name,
        kind: match target {
            Target::Cpu => BenchKind::Cpu,
            Target::Memory => BenchKind::Memory,
            Target::Gpu(_) => BenchKind::Gpu,
        },
        target,
        status: BenchStatus::Idle,
        rows: Vec::new(),
        stop: Arc::new(AtomicBool::new(false)),
    }
}

struct Reporter {
    slots: Arc<Mutex<Vec<BenchSlot>>>,
    index: usize,
}

impl Reporter {
    fn set_status(&self, status: BenchStatus) {
        self.slots.lock().unwrap()[self.index].status = status;
    }
    /// Record how a test ended, with its results, in the slot and the log.
    fn finish(&self, kind: &str, result: Result<(), String>) {
        {
            let slots = self.slots.lock().unwrap();
            let slot = &slots[self.index];
            let rows: Vec<String> = slot
                .rows
                .iter()
                .map(|r| format!("{}: {}", r.label, r.value))
                .collect();
            match &result {
                Ok(()) => log::info!("{kind} finished: {}: {}", slot.name, rows.join(", ")),
                Err(e) => log::warn!("{kind} failed: {}: {e}; {}", slot.name, rows.join(", ")),
            }
        }
        self.set_status(match result {
            Ok(()) => BenchStatus::Done,
            Err(e) => BenchStatus::Failed(e),
        });
    }
    fn step(&self, step: &'static str) {
        self.set_status(BenchStatus::Running(step));
    }
    fn row(&self, group: &'static str, label: &'static str, value: String) {
        self.slots.lock().unwrap()[self.index]
            .rows
            .push(BenchRow { group, label, value });
    }
    fn slot_name(&self) -> String {
        self.slots.lock().unwrap()[self.index].name.clone()
    }
    /// Replace everything under the stress-test heading.
    fn set_stress_rows(&self, rows: Vec<(&'static str, String)>) {
        let mut slots = self.slots.lock().unwrap();
        let slot = &mut slots[self.index];
        slot.rows.retain(|r| r.group != GROUP_STRESS);
        slot.rows.extend(rows.into_iter().map(|(label, value)| BenchRow {
            group: GROUP_STRESS,
            label,
            value,
        }));
    }
}

/// A random permutation forming one single cycle through all `n` nodes
/// (Sattolo's algorithm): `next[i]` is the node after `i`.
fn single_cycle(n: usize) -> Vec<u32> {
    let mut next: Vec<u32> = (0..n as u32).collect();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for i in (1..n).rev() {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % i as u64) as usize;
        next.swap(i, j);
    }
    next
}

// ---------------------------------------------------------------------------
// System memory
// ---------------------------------------------------------------------------

const MIB: usize = 1024 * 1024;
const RAM_MIN_ARRAY: usize = 16 * MIB;
const RAM_MAX_TOTAL: usize = 4096 * MIB;
const RAM_ROUNDS: usize = 3;
const RAM_PASSES: usize = 8;
const RAM_CHASE_BYTES: usize = 256 * MIB;
const CHASE_STRIDE: usize = 64; // one cache line per node

/// Zeroed, 64-byte aligned heap memory.
struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

// Each test thread only touches its own buffers.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    fn new(bytes: usize) -> Result<Self, String> {
        let layout = Layout::from_size_align(bytes, 64).map_err(|e| e.to_string())?;
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(format!("could not allocate {} MB", bytes / MIB));
        }
        Ok(Self { ptr, layout })
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

/// One thread's working set.
struct Arrays {
    a: AlignedBuf,
    b: AlignedBuf,
    c: AlignedBuf,
    len: usize,
}

const TRIAD_SCALAR: f64 = 3.0;

/// Loads `len` bytes (a multiple of 128) from `p`, 32 bytes at a time.
#[target_feature(enable = "avx2")]
unsafe fn read_avx2(p: *const u8, len: usize) {
    let end = p.add(len);
    asm!(
        "2:",
        "vmovdqa ymm0, ymmword ptr [{p}]",
        "vmovdqa ymm1, ymmword ptr [{p} + 32]",
        "vmovdqa ymm2, ymmword ptr [{p} + 64]",
        "vmovdqa ymm3, ymmword ptr [{p} + 96]",
        "add {p}, 128",
        "cmp {p}, {end}",
        "jb 2b",
        "vzeroupper",
        p = inout(reg) p => _,
        end = in(reg) end,
        out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _,
        options(nostack, readonly),
    );
}

/// STREAM Triad over f64 arrays: a[i] = b[i] + scalar * c[i].
#[target_feature(enable = "avx2")]
unsafe fn triad_avx2(a: *mut u8, b: *const u8, c: *const u8, len: usize, scalar: &f64) {
    asm!(
        "vbroadcastsd ymm0, qword ptr [{scalar}]",
        "xor {i:e}, {i:e}",
        "2:",
        "vmovapd ymm1, ymmword ptr [{c} + {i}]",
        "vmulpd ymm1, ymm1, ymm0",
        "vaddpd ymm1, ymm1, ymmword ptr [{b} + {i}]",
        "vmovapd ymmword ptr [{a} + {i}], ymm1",
        "add {i}, 32",
        "cmp {i}, {len}",
        "jb 2b",
        "vzeroupper",
        scalar = in(reg) scalar as *const f64,
        a = in(reg) a,
        b = in(reg) b,
        c = in(reg) c,
        len = in(reg) len,
        i = out(reg) _,
        out("ymm0") _, out("ymm1") _,
        options(nostack),
    );
}

/// Follows `hops` pointers starting at `p` and returns where it ended up.
unsafe fn chase(mut p: *const u8, hops: u64) -> *const u8 {
    asm!(
        "2:",
        "mov {p}, qword ptr [{p}]",
        "dec {n}",
        "jnz 2b",
        p = inout(reg) p,
        n = inout(reg) hops => _,
        options(nostack, readonly),
    );
    p
}

unsafe fn kernel_read(t: &Arrays) {
    read_avx2(t.a.ptr, t.len);
}
unsafe fn kernel_write(t: &Arrays) {
    std::ptr::write_bytes(t.a.ptr, 0x3F, t.len);
}
unsafe fn kernel_copy(t: &Arrays) {
    std::ptr::copy_nonoverlapping(t.b.ptr, t.a.ptr, t.len);
}
unsafe fn kernel_triad(t: &Arrays) {
    triad_avx2(t.a.ptr, t.b.ptr, t.c.ptr, t.len, &TRIAD_SCALAR);
}

/// Seconds for `threads` threads to all finish `work`, started together. Each
/// worker times itself: with every CPU busy, a separate timing thread could wake
/// late and undercount the elapsed time.
fn timed_parallel(threads: usize, work: impl Fn(usize) + Sync) -> f64 {
    let barrier = Barrier::new(threads);
    let spans: Vec<(Instant, Instant)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|index| {
                let (barrier, work) = (&barrier, &work);
                scope.spawn(move || {
                    barrier.wait();
                    let start = Instant::now();
                    work(index);
                    (start, Instant::now())
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let first_start = spans.iter().map(|s| s.0).min().unwrap();
    let last_end = spans.iter().map(|s| s.1).max().unwrap();
    (last_end - first_start).as_secs_f64()
}

/// Best GB/s over several rounds with all threads running `kernel` together.
fn ram_rate(arrays: &[Arrays], traffic_per_pass: usize, kernel: unsafe fn(&Arrays)) -> f64 {
    let mut best = 0.0f64;
    for _ in 0..RAM_ROUNDS {
        let secs = timed_parallel(arrays.len(), |index| {
            for _ in 0..RAM_PASSES {
                unsafe { kernel(&arrays[index]) };
            }
        });
        let bytes = traffic_per_pass * arrays.len() * RAM_PASSES;
        best = best.max(bytes as f64 / secs / 1e9);
    }
    best
}

/// Average ns per load, chasing a random cycle through `bytes` of memory. Whole
/// laps of the cycle must end exactly where they started.
fn chase_latency_ns(bytes: usize, min_hops: u64) -> Result<f64, String> {
    let nodes = bytes / CHASE_STRIDE;
    let buf = AlignedBuf::new(nodes * CHASE_STRIDE)?;
    let next = single_cycle(nodes);
    unsafe {
        for (i, &n) in next.iter().enumerate() {
            let slot = buf.ptr.add(i * CHASE_STRIDE) as *mut *const u8;
            *slot = buf.ptr.add(n as usize * CHASE_STRIDE);
        }
    }
    drop(next);

    let laps = min_hops.div_ceil(nodes as u64).max(1);
    let hops = laps * nodes as u64;
    let mut best = f64::MAX;
    for _ in 0..RAM_ROUNDS {
        let start = Instant::now();
        let end = unsafe { chase(buf.ptr, hops) };
        let secs = start.elapsed().as_secs_f64();
        if end != buf.ptr as *const u8 {
            return Err("latency check failed: pointer chase ended on the wrong node".to_string());
        }
        best = best.min(secs * 1e9 / hops as f64);
    }
    Ok(best)
}

fn run_memory(rep: &Reporter) -> Result<(), String> {
    if !is_x86_feature_detected!("avx2") {
        return Err("this test needs a CPU with AVX2".to_string());
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    // Stay within half of free memory: the test shouldn't push the machine into
    // paging, which would measure the disk instead.
    let free = sysload::memory()
        .map(|(_, used, total)| ((total - used).max(0.0) as f64 * 1e9) as usize)
        .unwrap_or(0);
    let budget = (free / 2).min(RAM_MAX_TOTAL);
    let len = (budget / threads / 3) & !4095;
    if len < RAM_MIN_ARRAY {
        return Err("not enough free memory to run this test".to_string());
    }

    rep.step("Allocating");
    let arrays = (0..threads)
        .map(|_| {
            Ok(Arrays {
                a: AlignedBuf::new(len)?,
                b: AlignedBuf::new(len)?,
                c: AlignedBuf::new(len)?,
                len,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    // Touch every page, with non-zero data (these bytes are also valid doubles).
    std::thread::scope(|scope| {
        for t in &arrays {
            scope.spawn(move || unsafe {
                std::ptr::write_bytes(t.a.ptr, 0x3F, len);
                std::ptr::write_bytes(t.b.ptr, 0x3F, len);
                std::ptr::write_bytes(t.c.ptr, 0x40, len);
            });
        }
    });

    let gbs = |v: f64| format!("{v:.1} GB/s");

    rep.step("Measuring read");
    rep.row(GROUP_BANDWIDTH, "Read", gbs(ram_rate(&arrays, len, kernel_read)));
    rep.step("Measuring write");
    rep.row(GROUP_BANDWIDTH, "Write", gbs(ram_rate(&arrays, len, kernel_write)));
    rep.step("Measuring copy");
    rep.row(GROUP_BANDWIDTH, "Copy", gbs(ram_rate(&arrays, len, kernel_copy)));

    rep.step("Measuring latency");
    rep.row(GROUP_LATENCY, "Random access", format!(
            "{:.1} ns",
            chase_latency_ns(RAM_CHASE_BYTES, (RAM_CHASE_BYTES / CHASE_STRIDE) as u64)?
        ));

    rep.step("Measuring throughput");
    // STREAM counts all three arrays, so this compares with STREAM's Triad figure.
    rep.row(
        GROUP_THROUGHPUT,
        "STREAM Triad",
        gbs(ram_rate(&arrays, len * 3, kernel_triad)),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

const CPU_ROUNDS: usize = 3;
const CACHE_READ_BYTES: usize = 40_000_000_000;
const CACHE_CHASE_HOPS: u64 = 20_000_000;
const INT_LOOPS: u64 = 1_000_000_000;
const INT_OPS_PER_LOOP: u64 = 4;
const FP_LOOPS: u64 = 300_000_000;
const FP_OPS_PER_LOOP: u64 = 16; // 4 AVX2 instructions x 4 doubles each

/// Data/unified cache sizes in bytes from CPUID, indexed by level - 1.
fn cache_sizes() -> [usize; 3] {
    use std::arch::x86_64::{__cpuid, __cpuid_count};
    let mut sizes = [0usize; 3];
    let leaf = {
        const AMD_CACHE_LEAF: u32 = 0x8000_001D;
        let vendor = __cpuid(0);
        let is_amd = vendor.ebx == u32::from_le_bytes(*b"Auth");
        if is_amd && __cpuid(0x8000_0000).eax >= AMD_CACHE_LEAF {
            AMD_CACHE_LEAF
        } else if !is_amd && vendor.eax >= 4 {
            4
        } else {
            return sizes;
        }
    };
    for sub in 0..16 {
        let r = __cpuid_count(leaf, sub);
        let kind = r.eax & 0x1F;
        if kind == 0 {
            break;
        }
        let level = ((r.eax >> 5) & 0x7) as usize;
        // 1 = data, 3 = unified; 2 is the instruction cache.
        if kind != 2 && (1..=3).contains(&level) {
            let ways = ((r.ebx >> 22) & 0x3FF) as usize + 1;
            let partitions = ((r.ebx >> 12) & 0x3FF) as usize + 1;
            let line = (r.ebx & 0xFFF) as usize + 1;
            let sets = r.ecx as usize + 1;
            sizes[level - 1] = ways * partitions * line * sets;
        }
    }
    sizes
}

/// `loops` iterations of four register additions. Returns the first counter.
unsafe fn int_kernel(loops: u64) -> u64 {
    let total: u64;
    asm!(
        "xor {a:e}, {a:e}",
        "xor {b:e}, {b:e}",
        "xor {c:e}, {c:e}",
        "xor {d:e}, {d:e}",
        "2:",
        "add {a}, 1",
        "add {b}, 2",
        "add {c}, 3",
        "add {d}, 4",
        "dec {n}",
        "jnz 2b",
        a = out(reg) total,
        b = out(reg) _,
        c = out(reg) _,
        d = out(reg) _,
        n = inout(reg) loops => _,
        options(nostack, nomem),
    );
    total
}

/// `loops` iterations of two multiplies and two adds on 4 doubles each. Every
/// instruction reads only the constant in ymm0, so the loop is limited by how
/// many the CPU can start per cycle rather than by waiting on earlier results.
#[target_feature(enable = "avx2")]
unsafe fn fp_kernel(loops: u64) {
    let one = 1.0f64;
    asm!(
        "vbroadcastsd ymm0, qword ptr [{one}]",
        "2:",
        "vmulpd ymm1, ymm0, ymm0",
        "vaddpd ymm2, ymm0, ymm0",
        "vmulpd ymm3, ymm0, ymm0",
        "vaddpd ymm4, ymm0, ymm0",
        "dec {n}",
        "jnz 2b",
        "vzeroupper",
        one = in(reg) &one as *const f64,
        n = inout(reg) loops => _,
        out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, out("ymm4") _,
        options(nostack, readonly),
    );
}

/// Best billions of operations per second across `threads` threads.
fn ops_rate(threads: usize, ops_per_thread: u64, work: impl Fn() + Sync) -> f64 {
    let mut best = 0.0f64;
    for _ in 0..CPU_ROUNDS {
        let secs = timed_parallel(threads, |_| work());
        best = best.max((ops_per_thread * threads as u64) as f64 / secs / 1e9);
    }
    best
}

/// Best single-thread read rate (GB/s) on a working set of `bytes`.
fn cache_read_gbs(bytes: usize) -> Result<f64, String> {
    let buf = AlignedBuf::new(bytes)?;
    unsafe { std::ptr::write_bytes(buf.ptr, 0x3F, bytes) };
    let passes = (CACHE_READ_BYTES / bytes).max(1);
    let mut best = 0.0f64;
    for _ in 0..CPU_ROUNDS {
        let start = Instant::now();
        for _ in 0..passes {
            unsafe { read_avx2(buf.ptr, bytes) };
        }
        let secs = start.elapsed().as_secs_f64();
        best = best.max((passes * bytes) as f64 / secs / 1e9);
    }
    Ok(best)
}

fn run_cpu(rep: &Reporter) -> Result<(), String> {
    if !is_x86_feature_detected!("avx2") {
        return Err("this test needs a CPU with AVX2".to_string());
    }
    const LEVELS: [(&str, &str, &str); 3] = [
        ("L1 read", "L1 access", "Measuring L1 cache"),
        ("L2 read", "L2 access", "Measuring L2 cache"),
        ("L3 read", "L3 access", "Measuring L3 cache"),
    ];
    // Half of each cache: it stays clear of the next level down, and leaves room
    // for whatever else the system is doing.
    let working_sets: Vec<usize> = cache_sizes()
        .iter()
        .map(|&size| (size / 2) & !127)
        .collect();

    for (&bytes, (read, _, step)) in working_sets.iter().zip(LEVELS) {
        if bytes > 0 {
            rep.step(step);
            let rate = cache_read_gbs(bytes)?;
            rep.row(GROUP_BANDWIDTH, read, format!("{rate:.0} GB/s"));
        }
    }
    for (&bytes, (_, access, step)) in working_sets.iter().zip(LEVELS) {
        if bytes > 0 {
            rep.step(step);
            let ns = chase_latency_ns(bytes, CACHE_CHASE_HOPS)?;
            rep.row(GROUP_LATENCY, access, format!("{ns:.1} ns"));
        }
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let int_ops = INT_LOOPS * INT_OPS_PER_LOOP;
    let int_work = || {
        std::hint::black_box(unsafe { int_kernel(INT_LOOPS) });
    };
    let fp_ops = FP_LOOPS * FP_OPS_PER_LOOP;
    let fp_work = || unsafe { fp_kernel(FP_LOOPS) };

    rep.step("Measuring integer throughput");
    rep.row(
        GROUP_THROUGHPUT,
        "Integer, 1 thread",
        format!("{:.1} G ops/s", ops_rate(1, int_ops, int_work)),
    );
    rep.row(
        GROUP_THROUGHPUT,
        "Integer, all threads",
        format!("{:.1} G ops/s", ops_rate(threads, int_ops, int_work)),
    );
    rep.step("Measuring floating-point throughput");
    rep.row(
        GROUP_THROUGHPUT,
        "Floating point, 1 thread",
        format!("{:.1} GFLOPS", ops_rate(1, fp_ops, fp_work)),
    );
    rep.row(
        GROUP_THROUGHPUT,
        "Floating point, all threads",
        format!("{:.1} GFLOPS", ops_rate(threads, fp_ops, fp_work)),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// GPU
// ---------------------------------------------------------------------------

const GPU_BUFFER_BYTES: u32 = 256 * MIB as u32;
const GPU_ROUNDS: usize = 8;
const GPU_FILL: u8 = 0x5A;
const GPU_CHASE_STRIDE: u32 = 256; // wider than any GPU cache line
const GPU_CHASE_HOPS: u32 = 200_000;
const GPU_CHASE_ROUNDS: usize = 5;
const GPU_ROUND_TRIPS: usize = 301;
const GPU_WAIT_LIMIT: Duration = Duration::from_secs(20);
const COPY_GROUP_THREADS: u32 = 1024;
const COPY_BYTES_PER_THREAD: u32 = 16;

const COPY_SHADER: &str = r#"
RWByteAddressBuffer src : register(u0);
RWByteAddressBuffer dst : register(u1);
[numthreads(1024, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint offset = id.x * 16;
    dst.Store4(offset, src.Load4(offset));
}
"#;

const CHASE_SHADER: &str = r#"
RWByteAddressBuffer nodes : register(u0);
RWByteAddressBuffer result : register(u1);
cbuffer Params : register(b0) { uint hops; };
[numthreads(1, 1, 1)]
void main() {
    uint p = 0;
    [loop] for (uint i = 0; i < hops; i++) {
        p = nodes.Load(p);
    }
    result.Store(0, p);
}
"#;

fn err(context: &str, e: windows::core::Error) -> String {
    format!("{context}: {}", e.message())
}

/// Hardware GPUs as (name, LUID). LUIDs stay valid while Windows is running,
/// unlike the enumeration order.
fn gpu_adapters() -> Vec<(String, LUID)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let end = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..end]);
            out.push((name.trim().to_string(), desc.AdapterLuid));
        }
    }
    out
}

fn find_adapter(luid: LUID) -> Result<IDXGIAdapter1, String> {
    unsafe {
        let factory =
            CreateDXGIFactory1::<IDXGIFactory1>().map_err(|e| err("DXGI unavailable", e))?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            if let Ok(desc) = adapter.GetDesc1() {
                if desc.AdapterLuid == luid {
                    return Ok(adapter);
                }
            }
        }
    }
    Err("this GPU is no longer available".to_string())
}

struct Gpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    query: ID3D11Query,
}

impl Gpu {
    fn new(adapter: &IDXGIAdapter1) -> Result<Self, String> {
        unsafe {
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(|e| err("could not open Direct3D 11", e))?;
            let device: ID3D11Device = device.ok_or("no Direct3D device")?;
            let ctx = ctx.ok_or("no Direct3D context")?;

            let desc = D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            };
            let mut query = None;
            device
                .CreateQuery(&desc, Some(&mut query))
                .map_err(|e| err("could not create a query", e))?;
            Ok(Self {
                device,
                ctx,
                query: query.ok_or("no Direct3D query")?,
            })
        }
    }

    /// Block until the GPU has finished everything submitted so far, spinning so
    /// that timings are accurate.
    fn wait(&self) -> Result<(), String> {
        self.wait_for_gpu(false)
    }

    /// As `wait`, but sleeping between checks: spinning would also load a CPU core.
    fn wait_relaxed(&self) -> Result<(), String> {
        self.wait_for_gpu(true)
    }

    fn wait_for_gpu(&self, relaxed: bool) -> Result<(), String> {
        unsafe {
            self.ctx.End(&self.query);
            self.ctx.Flush();
            let start = Instant::now();
            loop {
                let mut done = BOOL(0);
                self.ctx
                    .GetData(
                        &self.query,
                        Some(&mut done as *mut BOOL as *mut _),
                        std::mem::size_of::<BOOL>() as u32,
                        0,
                    )
                    .map_err(|e| err("the GPU stopped responding", e))?;
                if done.as_bool() {
                    return Ok(());
                }
                if start.elapsed() > GPU_WAIT_LIMIT {
                    return Err("timed out waiting for the GPU".to_string());
                }
                if relaxed {
                    std::thread::sleep(Duration::from_millis(1));
                } else {
                    std::hint::spin_loop();
                }
            }
        }
    }

    /// Seconds for `work` to finish on the GPU; the best of `rounds` runs.
    fn best_secs(&self, rounds: usize, work: impl Fn()) -> Result<f64, String> {
        let mut best = f64::MAX;
        for _ in 0..rounds {
            self.wait()?;
            let start = Instant::now();
            work();
            self.wait()?;
            best = best.min(start.elapsed().as_secs_f64());
        }
        Ok(best)
    }

    fn buffer(
        &self,
        bytes: u32,
        usage: D3D11_USAGE,
        bind: i32,
        cpu: i32,
        misc: i32,
    ) -> Result<ID3D11Buffer, String> {
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: bytes,
            Usage: usage,
            BindFlags: bind as u32,
            CPUAccessFlags: cpu as u32,
            MiscFlags: misc as u32,
            StructureByteStride: 0,
        };
        let mut buffer = None;
        unsafe {
            self.device
                .CreateBuffer(&desc, None, Some(&mut buffer))
                .map_err(|e| err(&format!("could not allocate {} MB", bytes as usize / MIB), e))?;
        }
        buffer.ok_or_else(|| "no Direct3D buffer".to_string())
    }

    /// GPU-local buffer that shaders can read and write as raw bytes.
    fn vram_buffer(&self, bytes: u32) -> Result<ID3D11Buffer, String> {
        self.buffer(
            bytes,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_UNORDERED_ACCESS.0 | D3D11_BIND_SHADER_RESOURCE.0,
            0,
            D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0,
        )
    }

    fn staging_buffer(&self, bytes: u32, cpu: D3D11_CPU_ACCESS_FLAG) -> Result<ID3D11Buffer, String> {
        self.buffer(bytes, D3D11_USAGE_STAGING, 0, cpu.0, 0)
    }

    fn uav(&self, buffer: &ID3D11Buffer, bytes: u32) -> Result<ID3D11UnorderedAccessView, String> {
        let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: bytes / 4,
                    Flags: D3D11_BUFFER_UAV_FLAG_RAW.0 as u32,
                },
            },
        };
        let mut view = None;
        unsafe {
            self.device
                .CreateUnorderedAccessView(buffer, Some(&desc), Some(&mut view))
                .map_err(|e| err("could not create a buffer view", e))?;
        }
        view.ok_or_else(|| "no Direct3D view".to_string())
    }

    fn shader(&self, source: &str) -> Result<ID3D11ComputeShader, String> {
        unsafe {
            let mut code = None;
            let mut errors = None;
            let compiled = D3DCompile(
                source.as_ptr() as *const _,
                source.len(),
                PCSTR::null(),
                None,
                None,
                s!("main"),
                s!("cs_5_0"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            );
            if let Err(e) = compiled {
                let detail = errors
                    .map(|b: windows::Win32::Graphics::Direct3D::ID3DBlob| {
                        let bytes = std::slice::from_raw_parts(
                            b.GetBufferPointer() as *const u8,
                            b.GetBufferSize(),
                        );
                        String::from_utf8_lossy(bytes).trim().to_string()
                    })
                    .unwrap_or_else(|| e.message());
                return Err(format!("shader compile failed: {detail}"));
            }
            let code: windows::Win32::Graphics::Direct3D::ID3DBlob =
                code.ok_or("shader compile returned nothing")?;
            let bytes =
                std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize());
            let mut shader = None;
            self.device
                .CreateComputeShader(bytes, None, Some(&mut shader))
                .map_err(|e| err("could not create a shader", e))?;
            shader.ok_or_else(|| "no Direct3D shader".to_string())
        }
    }

    fn bind_compute(
        &self,
        shader: Option<&ID3D11ComputeShader>,
        u0: Option<ID3D11UnorderedAccessView>,
        u1: Option<ID3D11UnorderedAccessView>,
    ) {
        let views = [u0, u1];
        unsafe {
            self.ctx.CSSetShader(shader, None);
            self.ctx
                .CSSetUnorderedAccessViews(0, 2, Some(views.as_ptr()), None);
        }
    }

    /// Calls `read` with the bytes of a CPU-readable staging buffer.
    fn read_staging<T>(
        &self,
        buffer: &ID3D11Buffer,
        bytes: u32,
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<T, String> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(buffer, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| err("could not read back from the GPU", e))?;
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, bytes as usize);
            let value = read(data);
            self.ctx.Unmap(buffer, 0);
            Ok(value)
        }
    }

    /// GPU memory access time (ns per hop), from a shader pointer chase.
    fn chase_ns(&self, nodes_buffer: &ID3D11Buffer) -> Result<f64, String> {
        let count = (GPU_BUFFER_BYTES / GPU_CHASE_STRIDE) as usize;
        let next = single_cycle(count);

        // Where the chase must end, worked out on the CPU.
        let mut expected = 0u32;
        for _ in 0..GPU_CHASE_HOPS {
            expected = next[expected as usize];
        }
        let expected_offset = expected * GPU_CHASE_STRIDE;

        // Node `i` lives at offset i * stride and holds the next node's offset.
        let mut image = vec![0u32; GPU_BUFFER_BYTES as usize / 4];
        let words_per_node = (GPU_CHASE_STRIDE / 4) as usize;
        for (i, &n) in next.iter().enumerate() {
            image[i * words_per_node] = n * GPU_CHASE_STRIDE;
        }

        let result = self.vram_buffer(16)?;
        let result_read = self.staging_buffer(16, D3D11_CPU_ACCESS_READ)?;
        let params = self.buffer(
            16,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_CONSTANT_BUFFER.0,
            0,
            0,
        )?;
        let params_data = [GPU_CHASE_HOPS, 0, 0, 0];
        let shader = self.shader(CHASE_SHADER)?;
        let nodes_view = self.uav(nodes_buffer, GPU_BUFFER_BYTES)?;
        let result_view = self.uav(&result, 16)?;

        unsafe {
            self.ctx
                .UpdateSubresource(nodes_buffer, 0, None, image.as_ptr() as *const _, 0, 0);
            self.ctx
                .UpdateSubresource(&params, 0, None, params_data.as_ptr() as *const _, 0, 0);
            self.ctx
                .CSSetConstantBuffers(0, Some(&[Some(params.clone())]));
        }
        drop(image);
        self.bind_compute(Some(&shader), Some(nodes_view), Some(result_view));

        let secs = self.best_secs(GPU_CHASE_ROUNDS, || unsafe {
            self.ctx.Dispatch(1, 1, 1);
        })?;

        self.bind_compute(None, None, None);
        unsafe {
            self.ctx.CopyResource(&result_read, &result);
        }
        self.wait()?;
        let landed = self.read_staging(&result_read, 16, |d| {
            u32::from_le_bytes([d[0], d[1], d[2], d[3]])
        })?;
        if landed != expected_offset {
            return Err("latency check failed: the chase ended on the wrong node".to_string());
        }
        Ok(secs * 1e9 / GPU_CHASE_HOPS as f64)
    }

    /// Median time (µs) to submit a tiny copy and hear back that it finished. It
    /// has to be real work: with nothing queued, the driver can answer without
    /// involving the GPU at all.
    fn round_trip_us(&self) -> Result<f64, String> {
        let src = self.vram_buffer(16)?;
        let dst = self.vram_buffer(16)?;
        self.wait()?;
        let mut samples = Vec::with_capacity(GPU_ROUND_TRIPS);
        for _ in 0..GPU_ROUND_TRIPS {
            let start = Instant::now();
            unsafe { self.ctx.CopyResource(&dst, &src) };
            self.wait()?;
            samples.push(start.elapsed().as_secs_f64() * 1e6);
        }
        samples.sort_by(f64::total_cmp);
        Ok(samples[samples.len() / 2])
    }
}

fn run_gpu(rep: &Reporter, luid: LUID) -> Result<(), String> {
    let adapter = find_adapter(luid)?;
    let gpu = Gpu::new(&adapter)?;
    let bytes = GPU_BUFFER_BYTES;
    let gbs = |secs: f64| format!("{:.1} GB/s", bytes as f64 / secs / 1e9);

    rep.step("Allocating");
    let upload = gpu.staging_buffer(bytes, D3D11_CPU_ACCESS_WRITE)?;
    let download = gpu.staging_buffer(bytes, D3D11_CPU_ACCESS_READ)?;
    let vram_a = gpu.vram_buffer(bytes)?;
    let vram_b = gpu.vram_buffer(bytes)?;
    unsafe {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx
            .Map(&upload, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
            .map_err(|e| err("could not write to the GPU", e))?;
        std::ptr::write_bytes(mapped.pData as *mut u8, GPU_FILL, bytes as usize);
        gpu.ctx.Unmap(&upload, 0);
    }

    rep.step("Measuring upload");
    let secs = gpu.best_secs(GPU_ROUNDS, || unsafe {
        gpu.ctx.CopyResource(&vram_a, &upload);
    })?;
    rep.row(GROUP_BANDWIDTH, "Upload (CPU to GPU)", gbs(secs));

    rep.step("Measuring download");
    let secs = gpu.best_secs(GPU_ROUNDS, || unsafe {
        gpu.ctx.CopyResource(&download, &vram_a);
    })?;
    rep.row(GROUP_BANDWIDTH, "Download (GPU to CPU)", gbs(secs));
    let intact = gpu.read_staging(&download, bytes, |d| {
        d[0] == GPU_FILL && d[d.len() / 2] == GPU_FILL && d[d.len() - 1] == GPU_FILL
    })?;
    if !intact {
        return Err("bandwidth check failed: downloaded data didn't match the upload".to_string());
    }

    rep.step("Measuring GPU memory copy");
    let secs = gpu.best_secs(GPU_ROUNDS, || unsafe {
        gpu.ctx.CopyResource(&vram_b, &vram_a);
    })?;
    let copy_engine = gbs(secs);

    rep.step("Measuring shader copy");
    let shader = gpu.shader(COPY_SHADER)?;
    let groups = bytes / COPY_BYTES_PER_THREAD / COPY_GROUP_THREADS;
    gpu.bind_compute(
        Some(&shader),
        Some(gpu.uav(&vram_a, bytes)?),
        Some(gpu.uav(&vram_b, bytes)?),
    );
    let secs = gpu.best_secs(GPU_ROUNDS, || unsafe {
        gpu.ctx.Dispatch(groups, 1, 1);
    })?;
    gpu.bind_compute(None, None, None);
    let shader_copy = gbs(secs);

    rep.step("Measuring latency");
    let chase_ns = gpu.chase_ns(&vram_a)?;
    rep.row(GROUP_LATENCY, "Memory access", format!("{chase_ns:.0} ns"));
    let round_trip = gpu.round_trip_us()?;
    rep.row(GROUP_LATENCY, "Command round trip", format!("{round_trip:.0} µs"));

    rep.row(GROUP_THROUGHPUT, "GPU memory copy", copy_engine);
    rep.row(GROUP_THROUGHPUT, "Shader copy", shader_copy);
    Ok(())
}

// ---------------------------------------------------------------------------
// Stress tests
// ---------------------------------------------------------------------------

// Past these, stop rather than wait for the hardware's own protection. Many CPUs
// are designed to run at 95 C under full load, so the CPU limit sits just above that.
const CPU_STRESS_TEMP_LIMIT: f32 = 98.0;
const GPU_STRESS_TEMP_LIMIT: f32 = 95.0;
const STRESS_TICK: Duration = Duration::from_secs(1);
// Clocks are still climbing just after the load starts, so the lowest clock is only
// tracked after this long; that way it reflects throttling.
const STRESS_CLOCK_SETTLE: Duration = Duration::from_secs(5);

// CPU: an arithmetic check small enough to stay in cache, plus raw AVX2 work.
const STRESS_CPU_CHECK_BYTES: usize = 128 * 1024;
const STRESS_CPU_FP_LOOPS: u64 = 2_000_000;

// GPU: 4 M invocations, tuned until one dispatch takes long enough to load the GPU
// but stays far below the 2 s after which Windows resets a GPU that seems hung.
const STRESS_GPU_RESULT_BYTES: u32 = 16 * MIB as u32;
const STRESS_GPU_GROUPS: u32 = 65536;
const STRESS_GPU_MIN_DISPATCH: Duration = Duration::from_millis(40);
const STRESS_GPU_MAX_ROUNDS: u32 = 1 << 16;
const STRESS_GPU_CHECK_EVERY: u32 = 10;

const STRESS_SHADER: &str = r#"
RWByteAddressBuffer result : register(u0);
cbuffer Params : register(b0) { uint rounds; };
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float a = 1.0 + id.x * 0.001;
    float b = 2.0 + id.x * 0.0007;
    uint hash = id.x;
    [loop] for (uint i = 0; i < rounds; i++) {
        a = sin(a) * cos(b) + sqrt(abs(a) + 1.0);
        b = frac(a * 1.37) + exp(-abs(b));
        hash = hash * 31 + asuint(a);
    }
    result.Store(id.x * 4, hash);
}
"#;

/// One reading of the device being stressed.
#[derive(Clone, Copy, Default)]
struct StressSample {
    temp: Option<f32>,
    clock_mhz: Option<f32>,
    power: Option<f32>,
    load: Option<f32>,
}

fn max_option(a: Option<f32>, b: Option<f32>) -> Option<f32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn min_option(a: Option<f32>, b: Option<f32>) -> Option<f32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[derive(Default)]
struct StressStats {
    peak_temp: Option<f32>,
    peak_power: Option<f32>,
    peak_clock: Option<f32>,
    low_clock: Option<f32>,
}

impl StressStats {
    fn add(&mut self, sample: StressSample, clocks_settled: bool) {
        self.peak_temp = max_option(self.peak_temp, sample.temp);
        self.peak_power = max_option(self.peak_power, sample.power);
        self.peak_clock = max_option(self.peak_clock, sample.clock_mhz);
        if clocks_settled {
            self.low_clock = min_option(self.low_clock, sample.clock_mhz);
        }
    }
}

fn minutes_seconds(d: Duration) -> String {
    format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60)
}

fn stress_rows(
    elapsed: Duration,
    sample: StressSample,
    stats: &StressStats,
    errors: u64,
) -> Vec<(&'static str, String)> {
    let temp = match (sample.temp, stats.peak_temp) {
        (Some(now), Some(peak)) => format!("{now:.0} °C (peak {peak:.0})"),
        _ => "no reading".to_string(),
    };
    let clock = match (sample.clock_mhz, stats.low_clock, stats.peak_clock) {
        (Some(now), Some(low), Some(peak)) => {
            format!("{now:.0} MHz ({low:.0}–{peak:.0})")
        }
        (Some(now), _, _) => format!("{now:.0} MHz"),
        _ => "no reading".to_string(),
    };
    let power = match (sample.power, stats.peak_power) {
        (Some(now), Some(peak)) => format!("{now:.0} W (peak {peak:.0})"),
        _ => "no reading".to_string(),
    };
    let load = sample
        .load
        .map_or("no reading".to_string(), |l| format!("{l:.0}%"));
    vec![
        ("Time", minutes_seconds(elapsed)),
        ("Temperature", temp),
        ("Clock", clock),
        ("Power", power),
        ("Load", load),
        ("Calculation errors", errors.to_string()),
    ]
}

/// How a stress test ended. Worker failures and errors outrank everything else,
/// since they're what the test exists to find.
fn stress_outcome(
    worker_error: Option<String>,
    errors: u64,
    hit_limit: Option<f32>,
) -> Result<&'static str, String> {
    if let Some(e) = worker_error {
        return Err(e);
    }
    if errors > 0 {
        return Err(format!(
            "{errors} calculation errors: this device may be unstable at these clocks."
        ));
    }
    if let Some(limit) = hit_limit {
        return Err(format!(
            "Stopped at the {limit:.0} °C safety limit. Check the cooling."
        ));
    }
    Ok("Stopped, no errors")
}

/// Runs until `stop` is set or the temperature limit is hit, showing live readings.
/// Sets `stop` on the way out, which ends the workers.
fn monitor_stress(
    rep: &Reporter,
    stop: &AtomicBool,
    temp_limit: f32,
    errors: &AtomicU64,
    worker_error: &Mutex<Option<String>>,
    read: impl Fn() -> StressSample,
) -> Result<(), String> {
    let start = Instant::now();
    let mut stats = StressStats::default();
    let mut hit_limit = None;
    let mut last_rows;

    loop {
        let elapsed = start.elapsed();
        let sample = read();
        stats.add(sample, elapsed >= STRESS_CLOCK_SETTLE);
        last_rows = stress_rows(
            elapsed,
            sample,
            &stats,
            errors.load(Ordering::SeqCst),
        );
        rep.set_stress_rows(last_rows.clone());

        if sample.temp.is_some_and(|t| t >= temp_limit) {
            hit_limit = Some(temp_limit);
            break;
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Sleep in small steps so Stop is quick.
        let tick_start = Instant::now();
        while tick_start.elapsed() < STRESS_TICK && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    stop.store(true, Ordering::SeqCst);

    let worker_error = worker_error.lock().unwrap().take();
    let outcome = stress_outcome(worker_error, errors.load(Ordering::SeqCst), hit_limit);
    if let Ok(result) = &outcome {
        last_rows.push(("Result", result.to_string()));
        rep.set_stress_rows(last_rows);
    }
    outcome.map(|_| ())
}

/// One CPU worker: recompute a known result, count any difference, and burn AVX2
/// work between checks. Below-normal priority keeps the app and the sensor readings
/// responsive; the CPU still runs flat out, as nothing else wants the time.
fn cpu_stress_worker(stop: &AtomicBool, errors: &AtomicU64) -> Result<(), String> {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    let len = STRESS_CPU_CHECK_BYTES;
    let a = AlignedBuf::new(len)?;
    let b = AlignedBuf::new(len)?;
    let c = AlignedBuf::new(len)?;
    let expected = AlignedBuf::new(len)?;
    unsafe {
        std::ptr::write_bytes(b.ptr, 0x3F, len);
        std::ptr::write_bytes(c.ptr, 0x40, len);
        triad_avx2(expected.ptr, b.ptr, c.ptr, len, &TRIAD_SCALAR);
        let expected = std::slice::from_raw_parts(expected.ptr, len);
        let result = std::slice::from_raw_parts(a.ptr as *const u8, len);
        while !stop.load(Ordering::SeqCst) {
            std::ptr::write_bytes(a.ptr, 0, len);
            triad_avx2(a.ptr, b.ptr, c.ptr, len, &TRIAD_SCALAR);
            if result != expected {
                errors.fetch_add(1, Ordering::SeqCst);
            }
            fp_kernel(STRESS_CPU_FP_LOOPS);
        }
    }
    Ok(())
}

fn stress_cpu(
    rep: &Reporter,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
) -> Result<(), String> {
    if !is_x86_feature_detected!("avx2") {
        return Err("this test needs a CPU with AVX2".to_string());
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let errors = AtomicU64::new(0);
    let worker_error = Mutex::new(None);

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let (errors, worker_error) = (&errors, &worker_error);
            scope.spawn(move || {
                if let Err(e) = cpu_stress_worker(stop, errors) {
                    *worker_error.lock().unwrap() = Some(e);
                    stop.store(true, Ordering::SeqCst);
                }
            });
        }
        monitor_stress(
            rep,
            stop,
            CPU_STRESS_TEMP_LIMIT,
            &errors,
            &worker_error,
            || {
                let s = shared.lock().unwrap();
                StressSample {
                    temp: s.cpu_temp,
                    clock_mhz: s.cpu_clock,
                    power: s.cpu_power,
                    load: s.cpu_load,
                }
            },
        )
    })
}

/// The GPU load: repeat a fixed compute job, comparing its output now and then with
/// the first run's. Runs on this thread because the device stays with one thread.
fn gpu_stress_worker(luid: LUID, stop: &AtomicBool, errors: &AtomicU64) -> Result<(), String> {
    let gpu = Gpu::new(&find_adapter(luid)?)?;
    let bytes = STRESS_GPU_RESULT_BYTES;
    let result = gpu.vram_buffer(bytes)?;
    let readback = gpu.staging_buffer(bytes, D3D11_CPU_ACCESS_READ)?;
    let params = gpu.buffer(16, D3D11_USAGE_DEFAULT, D3D11_BIND_CONSTANT_BUFFER.0, 0, 0)?;
    let shader = gpu.shader(STRESS_SHADER)?;
    let view = gpu.uav(&result, bytes)?;
    unsafe {
        gpu.ctx
            .CSSetConstantBuffers(0, Some(&[Some(params.clone())]));
    }
    gpu.bind_compute(Some(&shader), Some(view), None);

    let set_rounds = |rounds: u32| unsafe {
        let data = [rounds, 0, 0, 0];
        gpu.ctx
            .UpdateSubresource(&params, 0, None, data.as_ptr() as *const _, 0, 0);
    };

    // Find a workload big enough to keep the GPU busy. Doubling from below the target
    // can't overshoot by more than double, which stays far from Windows' hang limit.
    let mut rounds = 4;
    loop {
        set_rounds(rounds);
        let start = Instant::now();
        unsafe { gpu.ctx.Dispatch(STRESS_GPU_GROUPS, 1, 1) };
        gpu.wait_relaxed()?;
        if start.elapsed() >= STRESS_GPU_MIN_DISPATCH
            || rounds >= STRESS_GPU_MAX_ROUNDS
            || stop.load(Ordering::SeqCst)
        {
            break;
        }
        rounds *= 2;
    }

    let read_result = || -> Result<Vec<u8>, String> {
        unsafe { gpu.ctx.CopyResource(&readback, &result) };
        gpu.wait_relaxed()?;
        gpu.read_staging(&readback, bytes, |d| d.to_vec())
    };
    let expected = read_result()?;

    let mut dispatches = 0u32;
    while !stop.load(Ordering::SeqCst) {
        unsafe { gpu.ctx.Dispatch(STRESS_GPU_GROUPS, 1, 1) };
        gpu.wait_relaxed()?;
        dispatches += 1;
        if dispatches % STRESS_GPU_CHECK_EVERY == 0 && read_result()? != expected {
            errors.fetch_add(1, Ordering::SeqCst);
        }
    }
    gpu.bind_compute(None, None, None);
    Ok(())
}

fn stress_gpu(
    rep: &Reporter,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
    luid: LUID,
    name: &str,
) -> Result<(), String> {
    let errors = AtomicU64::new(0);
    let worker_error = Mutex::new(None);

    std::thread::scope(|scope| {
        {
            let (errors, worker_error) = (&errors, &worker_error);
            scope.spawn(move || {
                if let Err(e) = gpu_stress_worker(luid, stop, errors) {
                    *worker_error.lock().unwrap() = Some(e);
                    stop.store(true, Ordering::SeqCst);
                }
            });
        }
        monitor_stress(
            rep,
            stop,
            GPU_STRESS_TEMP_LIMIT,
            &errors,
            &worker_error,
            || {
                let s = shared.lock().unwrap();
                s.gpus
                    .iter()
                    .find(|g| g.name.eq_ignore_ascii_case(name))
                    .map(|g| StressSample {
                        temp: g.temp,
                        clock_mhz: g.clock,
                        power: g.power,
                        load: g.load,
                    })
                    .unwrap_or_default()
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stress_stats_track_peaks_and_ignore_early_clocks_for_the_low() {
        let sample = |temp, clock| StressSample {
            temp: Some(temp),
            clock_mhz: Some(clock),
            power: Some(temp),
            load: None,
        };
        let mut stats = StressStats::default();
        stats.add(sample(50.0, 400.0), false);
        stats.add(sample(80.0, 4900.0), true);
        stats.add(sample(70.0, 4300.0), true);
        assert_eq!(stats.peak_temp, Some(80.0));
        assert_eq!(stats.peak_clock, Some(4900.0));
        assert_eq!(stats.low_clock, Some(4300.0), "startup clock isn't throttling");
    }

    #[test]
    fn stress_rows_show_readings_or_say_there_are_none() {
        let mut stats = StressStats::default();
        let sample = StressSample {
            temp: Some(71.0),
            clock_mhz: Some(4500.0),
            power: None,
            load: Some(100.0),
        };
        stats.add(sample, true);
        let rows = stress_rows(
            Duration::from_secs(83),
            sample,
            &stats,
            0,
        );
        let get = |label| rows.iter().find(|r| r.0 == label).unwrap().1.as_str();
        assert_eq!(get("Time"), "1:23");
        assert_eq!(get("Temperature"), "71 °C (peak 71)");
        assert_eq!(get("Clock"), "4500 MHz (4500–4500)");
        assert_eq!(get("Power"), "no reading");
        assert_eq!(get("Load"), "100%");
        assert_eq!(get("Calculation errors"), "0");
    }

    #[test]
    fn stress_outcome_reports_the_most_serious_ending() {
        assert_eq!(stress_outcome(None, 0, None), Ok("Stopped, no errors"));
        assert!(stress_outcome(None, 0, Some(98.0)).unwrap_err().contains("98"));
        assert!(stress_outcome(None, 3, Some(98.0)).unwrap_err().contains("3 calculation"));
        assert_eq!(
            stress_outcome(Some("GPU reset".to_string()), 3, None),
            Err("GPU reset".to_string())
        );
    }

    fn test_reporter() -> Reporter {
        Reporter {
            slots: Arc::new(Mutex::new(vec![new_slot("test".to_string(), Target::Memory)])),
            index: 0,
        }
    }

    fn sample_at(temp: f32) -> StressSample {
        StressSample {
            temp: Some(temp),
            ..Default::default()
        }
    }

    #[test]
    fn stress_stops_itself_at_the_temperature_limit() {
        let rep = test_reporter();
        let stop = AtomicBool::new(false);
        let errors = AtomicU64::new(0);
        let worker_error = Mutex::new(None);
        let result = monitor_stress(&rep, &stop, 98.0, &errors, &worker_error, || sample_at(99.0));
        assert!(result.unwrap_err().contains("98"));
        assert!(stop.load(Ordering::SeqCst), "the workers must be told to stop");
    }

    #[test]
    fn stress_keeps_going_below_the_limit_until_stopped() {
        let rep = test_reporter();
        let stop = AtomicBool::new(false);
        let errors = AtomicU64::new(0);
        let worker_error = Mutex::new(None);
        let result = std::thread::scope(|scope| {
            let monitor = scope.spawn(|| {
                monitor_stress(&rep, &stop, 98.0, &errors, &worker_error, || sample_at(97.0))
            });
            std::thread::sleep(Duration::from_millis(300));
            assert!(!monitor.is_finished(), "it should run until told to stop");
            stop.store(true, Ordering::SeqCst);
            monitor.join().unwrap()
        });
        assert_eq!(result, Ok(()));
        let slots = rep.slots.lock().unwrap();
        let last = slots[0].rows.last().unwrap();
        assert_eq!((last.label, last.value.as_str()), ("Result", "Stopped, no errors"));
    }

    #[test]
    fn stress_reports_a_failure_from_its_workers() {
        let rep = test_reporter();
        let stop = AtomicBool::new(true);
        let errors = AtomicU64::new(0);
        let worker_error = Mutex::new(Some("the GPU stopped responding".to_string()));
        let result = monitor_stress(&rep, &stop, 98.0, &errors, &worker_error, || sample_at(50.0));
        assert_eq!(result, Err("the GPU stopped responding".to_string()));
    }

    /// The same speed tests as the Tests tab, printed.
    /// cargo test speed_report -- --ignored --nocapture
    #[test]
    #[ignore]
    fn speed_report() {
        let print = |name: &str, rep: &Reporter, result: Result<(), String>| {
            println!("REPORT {name}: {result:?}");
            for row in &rep.slots.lock().unwrap()[0].rows {
                println!("REPORT   {} / {}: {}", row.group, row.label, row.value);
            }
        };
        let rep = test_reporter();
        let result = run_memory(&rep);
        print("System memory", &rep, result);
        for (name, luid) in gpu_adapters() {
            let rep = test_reporter();
            let result = run_gpu(&rep, luid);
            print(&name, &rep, result);
        }
    }

    /// Real hardware under real load for a while, so not part of a normal test run:
    /// cargo test gpu_stress_on_real_hardware -- --ignored --nocapture
    #[test]
    #[ignore]
    fn gpu_stress_on_real_hardware() {
        for (name, luid) in gpu_adapters() {
            let stop = AtomicBool::new(false);
            let errors = AtomicU64::new(0);
            let result = std::thread::scope(|scope| {
                let worker = scope.spawn(|| gpu_stress_worker(luid, &stop, &errors));
                std::thread::sleep(Duration::from_secs(20));
                stop.store(true, Ordering::SeqCst);
                worker.join().unwrap()
            });
            println!("{name}: {result:?}, {} errors", errors.load(Ordering::SeqCst));
            result.unwrap();
            assert_eq!(errors.load(Ordering::SeqCst), 0, "{name}");
        }
    }

    #[test]
    fn cpu_stress_worker_stops_when_told_and_finds_no_errors() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let stop = AtomicBool::new(false);
        let errors = AtomicU64::new(0);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| cpu_stress_worker(&stop, &errors));
            std::thread::sleep(Duration::from_millis(100));
            stop.store(true, Ordering::SeqCst);
            worker.join().unwrap().unwrap();
        });
        assert_eq!(errors.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn single_cycle_visits_every_node_once() {
        let n = 10_000;
        let next = single_cycle(n);
        let mut seen = vec![false; n];
        let mut p = 0usize;
        for _ in 0..n {
            assert!(!seen[p], "revisited node {p} before finishing the lap");
            seen[p] = true;
            p = next[p] as usize;
        }
        assert_eq!(p, 0, "a full lap must return to the start");
        assert!(seen.iter().all(|&v| v));
    }

    #[test]
    fn triad_computes_b_plus_scalar_times_c() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let len = 4096;
        let (a, b, c) = (
            AlignedBuf::new(len).unwrap(),
            AlignedBuf::new(len).unwrap(),
            AlignedBuf::new(len).unwrap(),
        );
        let count = len / 8;
        unsafe {
            for i in 0..count {
                *(b.ptr as *mut f64).add(i) = i as f64;
                *(c.ptr as *mut f64).add(i) = 2.0;
            }
            triad_avx2(a.ptr, b.ptr, c.ptr, len, &TRIAD_SCALAR);
            for i in 0..count {
                assert_eq!(*(a.ptr as *const f64).add(i), i as f64 + 6.0);
            }
        }
    }

    #[test]
    fn chase_follows_the_links() {
        let stride = 64;
        let nodes = 1000;
        let buf = AlignedBuf::new(nodes * stride).unwrap();
        let next = single_cycle(nodes);
        unsafe {
            for (i, &n) in next.iter().enumerate() {
                *(buf.ptr.add(i * stride) as *mut *const u8) = buf.ptr.add(n as usize * stride);
            }
            let start = buf.ptr as *const u8;
            assert_eq!(chase(start, nodes as u64), start);
            assert_eq!(chase(start, 1), buf.ptr.add(next[0] as usize * stride) as *const u8);
        }
    }

    #[test]
    fn cache_sizes_grow_with_each_level() {
        let [l1, l2, l3] = cache_sizes();
        assert!(l1 >= 8 * 1024, "L1 was {l1} bytes");
        assert!(l2 > l1, "L2 {l2} should exceed L1 {l1}");
        assert!(l3 > l2, "L3 {l3} should exceed L2 {l2}");
    }

    #[test]
    fn int_kernel_runs_every_loop() {
        assert_eq!(unsafe { int_kernel(1234) }, 1234);
    }

    #[test]
    fn timed_parallel_runs_every_thread_and_includes_the_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let ran = AtomicUsize::new(0);
        let secs = timed_parallel(4, |_| {
            std::thread::sleep(Duration::from_millis(30));
            ran.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(ran.load(Ordering::SeqCst), 4);
        assert!(secs >= 0.03, "measured only {secs} s");
    }

    #[test]
    fn read_kernel_covers_the_whole_buffer_without_faulting() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let buf = AlignedBuf::new(4096).unwrap();
        unsafe { read_avx2(buf.ptr, 4096) };
    }
}

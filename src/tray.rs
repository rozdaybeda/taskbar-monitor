// System tray icon and floating desktop widget. The widget shows whichever readings
// the user picks from its menu; the tray icon is static, with readings in its tooltip.
//
// Both are plain Win32 on their own thread with their own message loop, not egui
// windows, so they stay light while the main window sits in the tray.
//
// The widget has three positions. Floating is a free window on the desktop. The other
// two put it on the taskbar, because Windows 11 has no supported way to do that (it
// removed DeskBands), and each has a trade-off:
//   Overlay:  a separate window placed over the taskbar and owned by it, which is what
//             keeps it above the taskbar when the taskbar takes focus. It has to hide
//             itself for full-screen apps and an auto-hiding taskbar.
//   Embedded: a child window of Explorer's taskbar. It moves, stacks and hides with the
//             taskbar on its own, but relies on an undocumented arrangement of windows
//             that a Windows update could change.
//
// The widget is a layered window, drawn in software
// into a premultiplied-alpha bitmap, which is what gives it real transparency.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE,
    NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW,
    QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::logging;
use crate::sensors::Sampler;
use crate::{EnablePhase, GpuState, Shared};

const WM_TRAY: u32 = WM_APP + 1;
const WM_SHOW_MAIN: u32 = WM_APP + 2;
const WM_QUIT_APP: u32 = WM_APP + 3;
const WM_UPDATE_FOUND: u32 = WM_APP + 4;
const TIMER_REFRESH: usize = 1;
const REFRESH_MS: u32 = 1000;
const TRAY_CLASS: PCWSTR = w!("MonitorTrayWindow");
const WIDGET_CLASS: PCWSTR = w!("MonitorWidgetWindow");

const MENU_OPEN: usize = 1;
const MENU_WIDGET: usize = 2;
const MENU_ENABLE: usize = 3;
const MENU_QUIT: usize = 4;
const MENU_UPDATE: usize = 5;
const MENU_MODE_BASE: usize = 50;
const MENU_ITEM_BASE: usize = 100;

// Widget layout at 100% display scale.
const WIDGET_W: f32 = 192.0;
const WIDGET_PAD_X: f32 = 12.0;
const WIDGET_PAD_Y: f32 = 8.0;
const WIDGET_ROW_H: f32 = 20.0;
const WIDGET_TEXT_PX: f32 = 13.0;
const WIDGET_RADIUS: f32 = 10.0;
const WIDGET_MARGIN: i32 = 16;
const PANEL: Rgba = Rgba(28, 30, 36, 165);
const PANEL_EDGE: Rgba = Rgba(255, 255, 255, 40);
const TEXT_MAIN: Rgba = Rgba(235, 237, 240, 255);
const TEXT_DIM: Rgba = Rgba(190, 195, 203, 255);
const ACCENT_OFF: Rgba = Rgba(235, 175, 105, 255);
const NO_VALUE: &str = "—";

// Taskbar layout at 100% display scale: rows stack in pairs, in columns.
const TASKBAR_ROWS_PER_COLUMN: usize = 2;
const TASKBAR_TEXT_PX: f32 = 12.0;
const TASKBAR_ROW_H: f32 = 17.0;
const TASKBAR_LABEL_GAP: f32 = 6.0;
const TASKBAR_COLUMN_GAP: f32 = 14.0;
const TASKBAR_PAD_X: f32 = 6.0;
const TASKBAR_TRAY_GAP: i32 = 4;
// Almost invisible, but pixels with no alpha at all don't receive mouse clicks.
const TASKBAR_HIT_AREA: Rgba = Rgba(0, 0, 0, 1);
const TASKBAR_LIGHT_TEXT: Rgba = Rgba(26, 26, 28, 255);
const TASKBAR_LIGHT_DIM: Rgba = Rgba(84, 86, 92, 255);
const SCREEN_EDGE_TOLERANCE: i32 = 4;

/// What the tray thread needs from the rest of the app.
pub struct TrayContext {
    pub shared: Arc<Mutex<Shared>>,
    pub egui_ctx: egui::Context,
    pub main_hwnd: isize,
    pub sampler: Arc<Mutex<Option<Sampler>>>,
    pub enable_phase: Arc<Mutex<EnablePhase>>,
}

static CONTEXT: OnceLock<TrayContext> = OnceLock::new();
static TRAY_READY: AtomicBool = AtomicBool::new(false);
static MAIN_HIDDEN: AtomicBool = AtomicBool::new(false);

/// True once the tray icon exists, meaning closing the main window can safely
/// hide it instead of exiting: there's a way to bring it back.
pub fn is_ready() -> bool {
    TRAY_READY.load(Ordering::SeqCst)
}

pub fn start(context: TrayContext) {
    if CONTEXT.set(context).is_err() {
        return;
    }
    std::thread::spawn(|| unsafe { run() });
}

/// Ask the instance that's already running to show its main window.
pub fn signal_running_instance() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(TRAY_CLASS, PCWSTR::null()) {
            let _ = PostMessageW(hwnd, WM_SHOW_MAIN, WPARAM(0), LPARAM(0));
        }
    }
}

/// Ask the running instance to exit, and wait until it has, so its exe can be replaced.
pub fn quit_running_instance() {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    unsafe {
        let Ok(hwnd) = FindWindowW(TRAY_CLASS, PCWSTR::null()) else {
            return;
        };
        let _ = PostMessageW(hwnd, WM_QUIT_APP, WPARAM(0), LPARAM(0));
        let start = std::time::Instant::now();
        while start.elapsed() < WAIT && FindWindowW(TRAY_CLASS, PCWSTR::null()).is_ok() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// True while the main window is sent to the tray.
pub fn main_hidden() -> bool {
    MAIN_HIDDEN.load(Ordering::SeqCst)
}

unsafe fn set_cloaked(hwnd: HWND, cloaked: bool) {
    let value = BOOL(cloaked as i32);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_CLOAK,
        &value as *const BOOL as *const c_void,
        std::mem::size_of::<BOOL>() as u32,
    );
}

unsafe fn set_tool_window(hwnd: HWND, tool: bool) {
    let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    let flag = WS_EX_TOOLWINDOW.0 as isize;
    let style = if tool { style | flag } else { style & !flag };
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
}

/// Sends the main window to the tray by cloaking it, not with SW_HIDE. Hidden
/// windows get no WM_PAINT, and eframe polls in a busy loop while it waits for a
/// redraw it has asked for, which would use a whole CPU core. A cloaked window is
/// invisible yet still paints. Tool windows stay off the taskbar, and that style
/// only takes effect across a hide and show, which the cloak keeps out of sight.
pub fn hide_main_window(main_hwnd: isize) {
    let hwnd = HWND(main_hwnd as *mut c_void);
    unsafe {
        set_cloaked(hwnd, true);
        let _ = ShowWindow(hwnd, SW_HIDE);
        set_tool_window(hwnd, true);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    MAIN_HIDDEN.store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Readings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum TempState {
    Live(i32),
    /// Sensors are off, and a click can install the service.
    Off,
    /// The service is being installed or started.
    Starting,
    /// No temperature, and installing the service wouldn't help.
    Unavailable,
}

fn temp_state(
    temp: Option<f32>,
    service_alive: bool,
    phase: EnablePhase,
) -> TempState {
    match temp {
        Some(t) => TempState::Live(t.round() as i32),
        None if service_alive => TempState::Unavailable,
        None if phase != EnablePhase::Idle => TempState::Starting,
        None => TempState::Off,
    }
}

fn tooltip(state: TempState, load: Option<i32>) -> String {
    let load = load.map(|l| format!(" · Load {l}%")).unwrap_or_default();
    match state {
        TempState::Live(t) => format!("CPU {t} °C{load}"),
        TempState::Off => format!("CPU temperature off{load}"),
        TempState::Starting => format!("Starting CPU sensors…{load}"),
        TempState::Unavailable => format!("CPU temperature unavailable{load}"),
    }
}

/// Which GPU reading a widget row shows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum GpuMetric {
    Temp,
    Load,
    Clock,
    Power,
    Vram,
}

const GPU_METRICS: [GpuMetric; 5] = [
    GpuMetric::Temp,
    GpuMetric::Load,
    GpuMetric::Clock,
    GpuMetric::Power,
    GpuMetric::Vram,
];

/// One selectable widget row. The declaration order is the display order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum WidgetItem {
    CpuTemp,
    CpuLoad,
    CpuFreq,
    CpuPower,
    Gpu(usize, GpuMetric),
    Memory,
}

impl WidgetItem {
    /// Readings that only exist while the sensor service is running.
    fn needs_sensors(self) -> bool {
        matches!(self, Self::CpuTemp | Self::CpuFreq | Self::CpuPower)
    }

    fn token(self) -> String {
        match self {
            Self::CpuTemp => "cpu_temp".to_string(),
            Self::CpuLoad => "cpu_load".to_string(),
            Self::CpuFreq => "cpu_freq".to_string(),
            Self::CpuPower => "cpu_power".to_string(),
            Self::Memory => "memory".to_string(),
            Self::Gpu(i, metric) => {
                let name = match metric {
                    GpuMetric::Temp => "temp",
                    GpuMetric::Load => "load",
                    GpuMetric::Clock => "clock",
                    GpuMetric::Power => "power",
                    GpuMetric::Vram => "vram",
                };
                format!("gpu{i}_{name}")
            }
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        Some(match token {
            "cpu_temp" => Self::CpuTemp,
            "cpu_load" => Self::CpuLoad,
            "cpu_freq" => Self::CpuFreq,
            "cpu_power" => Self::CpuPower,
            "memory" => Self::Memory,
            _ => {
                let (index, name) = token.strip_prefix("gpu")?.split_once('_')?;
                let metric = match name {
                    "temp" => GpuMetric::Temp,
                    "load" => GpuMetric::Load,
                    "clock" => GpuMetric::Clock,
                    "power" => GpuMetric::Power,
                    "vram" => GpuMetric::Vram,
                    _ => return None,
                };
                Self::Gpu(index.parse().ok()?, metric)
            }
        })
    }

    /// Short name for the widget row.
    fn row_label(self, gpu_count: usize) -> String {
        match self {
            Self::CpuTemp => "CPU temp".to_string(),
            Self::CpuLoad => "CPU load".to_string(),
            Self::CpuFreq => "CPU freq".to_string(),
            Self::CpuPower => "CPU power".to_string(),
            Self::Memory => "Memory".to_string(),
            Self::Gpu(i, metric) => {
                let name = match metric {
                    GpuMetric::Temp => "temp",
                    GpuMetric::Load => "load",
                    GpuMetric::Clock => "clock",
                    GpuMetric::Power => "power",
                    GpuMetric::Vram => "VRAM",
                };
                format!("{} {name}", gpu_title(i, gpu_count))
            }
        }
    }

    /// Full name for the menu.
    fn menu_label(self, gpu_count: usize) -> String {
        match self {
            Self::CpuTemp => "CPU temperature".to_string(),
            Self::CpuLoad => "CPU load".to_string(),
            Self::CpuFreq => "CPU frequency".to_string(),
            Self::CpuPower => "CPU power".to_string(),
            Self::Memory => "Memory used".to_string(),
            Self::Gpu(i, metric) => {
                let name = match metric {
                    GpuMetric::Temp => "temperature",
                    GpuMetric::Load => "load",
                    GpuMetric::Clock => "clock",
                    GpuMetric::Power => "power",
                    GpuMetric::Vram => "VRAM used",
                };
                format!("{} {name}", gpu_title(i, gpu_count))
            }
        }
    }

    /// Items in the same group sit together in the menu, between separators.
    fn group(self) -> usize {
        match self {
            Self::CpuTemp | Self::CpuLoad | Self::CpuFreq | Self::CpuPower => 0,
            Self::Gpu(i, _) => i + 1,
            Self::Memory => usize::MAX,
        }
    }
}

/// Where the widget lives; see the top of this file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WidgetMode {
    Floating,
    TaskbarOverlay,
    TaskbarEmbedded,
}

const WIDGET_MODES: [WidgetMode; 3] = [
    WidgetMode::Floating,
    WidgetMode::TaskbarOverlay,
    WidgetMode::TaskbarEmbedded,
];

impl WidgetMode {
    fn token(self) -> &'static str {
        match self {
            Self::Floating => "floating",
            Self::TaskbarOverlay => "overlay",
            Self::TaskbarEmbedded => "embedded",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        WIDGET_MODES.into_iter().find(|m| m.token() == token)
    }

    fn menu_label(self) -> PCWSTR {
        match self {
            Self::Floating => w!("Floating on desktop"),
            Self::TaskbarOverlay => w!("Taskbar (overlay)"),
            Self::TaskbarEmbedded => w!("Taskbar (embedded)"),
        }
    }

    fn on_taskbar(self) -> bool {
        self != Self::Floating
    }
}

/// Same names as the main window's cards.
fn gpu_title(index: usize, gpu_count: usize) -> String {
    if gpu_count > 1 {
        format!("GPU {}", index + 1)
    } else {
        "GPU".to_string()
    }
}

fn default_items() -> Vec<WidgetItem> {
    vec![
        WidgetItem::CpuTemp,
        WidgetItem::CpuLoad,
        WidgetItem::CpuPower,
        WidgetItem::Memory,
    ]
}

/// Overlay rather than embedded because it doesn't depend on Explorer's internals.
const DEFAULT_WIDGET_MODE: WidgetMode = WidgetMode::TaskbarOverlay;

/// Everything the menu offers on a machine with `gpu_count` GPUs.
fn all_items(gpu_count: usize) -> Vec<WidgetItem> {
    let mut items = vec![
        WidgetItem::CpuTemp,
        WidgetItem::CpuLoad,
        WidgetItem::CpuFreq,
        WidgetItem::CpuPower,
    ];
    for i in 0..gpu_count {
        items.extend(GPU_METRICS.map(|metric| WidgetItem::Gpu(i, metric)));
    }
    items.push(WidgetItem::Memory);
    items
}

/// `items` with `item` switched on or off, kept in display order. The last item
/// can't be switched off: an empty widget would have nothing to right-click on
/// that explains itself.
fn toggled(items: &[WidgetItem], item: WidgetItem) -> Vec<WidgetItem> {
    let mut next = items.to_vec();
    if let Some(position) = next.iter().position(|&i| i == item) {
        if next.len() > 1 {
            next.remove(position);
        }
    } else {
        next.push(item);
        next.sort();
    }
    next
}

#[derive(Clone)]
struct Readings {
    state: TempState,
    cpu_load: Option<f32>,
    cpu_mhz: Option<f32>,
    cpu_power: Option<f32>,
    gpus: Vec<GpuState>,
    mem_used_gb: f32,
    mem_total_gb: f32,
}

fn read_readings() -> Readings {
    let context = CONTEXT.get().unwrap();
    let (temp, alive, mut readings) = {
        let s = context.shared.lock().unwrap();
        (
            s.cpu_temp,
            s.sensor_service_alive,
            Readings {
                state: TempState::Off,
                cpu_load: s.cpu_load,
                cpu_mhz: s.cpu_clock,
                cpu_power: s.cpu_power,
                gpus: s.gpus.clone(),
                mem_used_gb: s.mem_used_gb,
                mem_total_gb: s.mem_total_gb,
            },
        )
    };
    let phase = crate::current_enable_phase(&context.enable_phase, alive);
    readings.state = temp_state(temp, alive, phase);
    readings
}

#[derive(Clone, PartialEq, Debug)]
struct Row {
    label: String,
    value: String,
    color: Rgba,
}

fn percent(v: Option<f32>) -> String {
    v.map_or(NO_VALUE.to_string(), |v| format!("{v:.0}%"))
}

fn watts(v: Option<f32>) -> String {
    v.map_or(NO_VALUE.to_string(), |v| format!("{v:.0} W"))
}

fn temp_row_value(v: Option<f32>) -> (String, Rgba) {
    match v {
        Some(t) => {
            let t = t.round() as i32;
            (format!("{t} °C"), temp_rgba(t))
        }
        None => (NO_VALUE.to_string(), TEXT_MAIN),
    }
}

/// The widget's rows. While the sensors are off, the first row that needs them
/// says what to do and any others stay blank, rather than repeating it.
fn widget_rows(items: &[WidgetItem], r: &Readings) -> Vec<Row> {
    let gpu_count = r.gpus.len();
    let mut hint_shown = false;
    let mut rows = Vec::new();
    for &item in items {
        let (mut value, mut color) = (NO_VALUE.to_string(), TEXT_MAIN);
        match item {
            WidgetItem::CpuTemp => {
                if let TempState::Live(t) = r.state {
                    (value, color) = (format!("{t} °C"), temp_rgba(t));
                }
            }
            WidgetItem::CpuLoad => value = percent(r.cpu_load),
            WidgetItem::CpuFreq => {
                if let Some(mhz) = r.cpu_mhz {
                    value = format!("{:.2} GHz", mhz / 1000.0);
                }
            }
            WidgetItem::CpuPower => value = watts(r.cpu_power),
            WidgetItem::Memory => {
                if r.mem_total_gb > 0.0 {
                    value = format!("{:.1} / {:.0} GB", r.mem_used_gb, r.mem_total_gb);
                }
            }
            WidgetItem::Gpu(i, metric) => {
                // A saved choice can name a GPU that's since been removed.
                let Some(gpu) = r.gpus.get(i) else {
                    continue;
                };
                match metric {
                    GpuMetric::Temp => (value, color) = temp_row_value(gpu.temp),
                    GpuMetric::Load => value = percent(gpu.load),
                    GpuMetric::Clock => {
                        if let Some(mhz) = gpu.clock {
                            value = format!("{mhz:.0} MHz");
                        }
                    }
                    GpuMetric::Power => value = watts(gpu.power),
                    GpuMetric::Vram => {
                        if let (Some(used), Some(total)) = (gpu.mem_used_mb, gpu.mem_total_mb) {
                            value = format!("{:.1} / {:.0} GB", used / 1024.0, total / 1024.0);
                        }
                    }
                }
            }
        }
        if item.needs_sensors() && !hint_shown {
            let hint = match r.state {
                TempState::Off => Some("Click to enable"),
                TempState::Starting => Some("Approve prompt"),
                _ => None,
            };
            if let Some(hint) = hint {
                (value, color) = (hint.to_string(), ACCENT_OFF);
                hint_shown = true;
            }
        }
        rows.push(Row {
            label: item.row_label(gpu_count),
            value,
            color,
        });
    }
    if rows.is_empty() {
        rows.push(Row {
            label: "Monitor".to_string(),
            value: NO_VALUE.to_string(),
            color: TEXT_MAIN,
        });
    }
    rows
}

fn temp_rgba(t: i32) -> Rgba {
    let c = crate::temp_color(t as f32);
    Rgba(c.r(), c.g(), c.b(), 255)
}

// ---------------------------------------------------------------------------
// Software drawing
// ---------------------------------------------------------------------------

/// A colour with straight (not premultiplied) alpha.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Rgba(u8, u8, u8, u8);

/// Top-down pixels as BGRA with premultiplied alpha, as layered windows expect.
struct Canvas {
    w: i32,
    h: i32,
    px: Vec<[u8; 4]>,
}

/// How much of the pixel at distance `d` from a shape's edge lies inside it.
fn edge_coverage(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

/// Signed distance from (x, y) to a rounded rectangle; negative is inside.
fn rounded_distance(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let qx = (x - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (y - h / 2.0).abs() - (h / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

impl Canvas {
    fn new(w: i32, h: i32) -> Self {
        Self {
            w,
            h,
            px: vec![[0; 4]; (w * h) as usize],
        }
    }

    fn blend(&mut self, x: i32, y: i32, color: Rgba, coverage: f32) {
        let a = color.3 as f32 / 255.0 * coverage;
        let p = &mut self.px[(y * self.w + x) as usize];
        let channel = |src: u8, dst: u8| (src as f32 * a + dst as f32 * (1.0 - a)).round() as u8;
        *p = [
            channel(color.2, p[0]),
            channel(color.1, p[1]),
            channel(color.0, p[2]),
            (a * 255.0 + p[3] as f32 * (1.0 - a)).round() as u8,
        ];
    }

    /// Fill the whole canvas with a rounded rectangle plus a thin inner edge.
    fn fill_panel(&mut self, radius: f32, fill: Rgba, edge: Rgba, edge_width: f32) {
        let (w, h) = (self.w as f32, self.h as f32);
        for y in 0..self.h {
            for x in 0..self.w {
                let d = rounded_distance(x as f32 + 0.5, y as f32 + 0.5, w, h, radius);
                let outer = edge_coverage(d);
                let inner = edge_coverage(d + edge_width);
                self.blend(x, y, fill, outer);
                self.blend(x, y, edge, outer - inner);
            }
        }
    }
}

/// A top-down 32-bit bitmap selected into a memory DC, freed on drop.
struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut [u8; 4],
    len: usize,
}

impl Dib {
    unsafe fn new(w: i32, h: i32) -> Option<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(None);
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let previous = SelectObject(dc, bitmap);
        Some(Self {
            dc,
            bitmap,
            previous,
            bits: bits as *mut [u8; 4],
            len: (w * h) as usize,
        })
    }

    fn pixels(&self) -> &[[u8; 4]] {
        unsafe { std::slice::from_raw_parts(self.bits, self.len) }
    }

    fn pixels_mut(&mut self) -> &mut [[u8; 4]] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.dc);
        }
    }
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

struct TextStyle {
    px: i32,
    bold: bool,
    color: Rgba,
}

unsafe fn make_font(px: i32, bold: bool) -> HFONT {
    CreateFontW(
        -px,
        0,
        0,
        0,
        if bold { 700 } else { 400 },
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        // Grayscale antialiasing: ClearType's colour fringes need an opaque
        // background, and this canvas is translucent.
        ANTIALIASED_QUALITY.0 as u32,
        0,
        w!("Segoe UI"),
    )
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// (width in pixels) of `text`.
unsafe fn text_width(text: &str, px: i32, bold: bool) -> i32 {
    let Some(dib) = Dib::new(1, 1) else {
        return 0;
    };
    let font = make_font(px, bold);
    let previous = SelectObject(dib.dc, font);
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(dib.dc, &wide(text), &mut size);
    SelectObject(dib.dc, previous);
    let _ = DeleteObject(font);
    size.cx
}

/// GDI can't draw onto alpha, so draw white on black off to the side and use the
/// result as the text's coverage mask.
unsafe fn draw_text(canvas: &mut Canvas, text: &str, x: i32, y: i32, align: Align, style: &TextStyle) {
    let Some(mut dib) = Dib::new(canvas.w, canvas.h) else {
        return;
    };
    dib.pixels_mut().fill([0; 4]);
    let font = make_font(style.px, style.bold);
    let previous = SelectObject(dib.dc, font);
    SetBkMode(dib.dc, TRANSPARENT);
    SetTextColor(dib.dc, COLORREF(0x00FF_FFFF));
    let chars = wide(text);
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(dib.dc, &chars, &mut size);
    let left = match align {
        Align::Left => x,
        Align::Right => x - size.cx,
    };
    let _ = TextOutW(dib.dc, left, y, &chars);
    SelectObject(dib.dc, previous);
    let _ = DeleteObject(font);

    for py in 0..canvas.h {
        for px in 0..canvas.w {
            let m = dib.pixels()[(py * canvas.w + px) as usize];
            let coverage = m[0].max(m[1]).max(m[2]);
            if coverage > 0 {
                canvas.blend(px, py, style.color, coverage as f32 / 255.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Thread and tray icon
// ---------------------------------------------------------------------------

struct UiState {
    tray_hwnd: Cell<HWND>,
    widget_hwnd: Cell<HWND>,
    widget_visible: Cell<bool>,
    widget_mode: Cell<WidgetMode>,
    floating_position: Cell<Option<(i32, i32)>>,
    app_icon: Cell<HICON>,
    taskbar_created: Cell<u32>,
    items: RefCell<Vec<WidgetItem>>,
    shown_tip: RefCell<Option<String>>,
    shown_widget: RefCell<Option<WidgetKey>>,
}

/// Everything that decides how the widget looks, to skip redrawing when it's unchanged.
#[derive(PartialEq)]
struct WidgetKey {
    rows: Vec<Row>,
    dpi: u32,
    mode: WidgetMode,
    light_taskbar: bool,
    height: i32,
}

thread_local! {
    static UI: UiState = UiState {
        tray_hwnd: Cell::new(HWND::default()),
        widget_hwnd: Cell::new(HWND::default()),
        widget_visible: Cell::new(true),
        widget_mode: Cell::new(DEFAULT_WIDGET_MODE),
        floating_position: Cell::new(None),
        app_icon: Cell::new(HICON::default()),
        taskbar_created: Cell::new(0),
        items: RefCell::new(default_items()),
        shown_tip: RefCell::new(None),
        shown_widget: RefCell::new(None),
    };
}

unsafe fn run() {
    let Ok(module) = GetModuleHandleW(None) else {
        return;
    };
    let instance = module.into();
    let cursor = LoadCursorW(None, IDC_HAND).unwrap_or_default();

    RegisterClassW(&WNDCLASSW {
        lpfnWndProc: Some(tray_proc),
        hInstance: instance,
        lpszClassName: TRAY_CLASS,
        ..Default::default()
    });
    RegisterClassW(&WNDCLASSW {
        lpfnWndProc: Some(widget_proc),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: WIDGET_CLASS,
        ..Default::default()
    });

    // A hidden top-level window rather than a message-only one: only top-level
    // windows hear TaskbarCreated, which says Explorer restarted.
    let Ok(tray_hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        TRAY_CLASS,
        w!("Monitor tray"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        instance,
        None,
    ) else {
        return;
    };

    set_main_window_icons(instance);
    // The exact tray size from the icon file, drawn for that size; the built-in
    // bitmap is only the fallback for builds made without a resource compiler.
    let app_icon = load_icon_resource(instance, GetSystemMetrics(SM_CXSMICON)).or_else(|| {
        let icon = crate::app_icon();
        let bgra: Vec<[u8; 4]> = icon
            .rgba
            .chunks_exact(4)
            .map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        make_icon(icon.width as i32, icon.height as i32, &bgra)
    });

    let settings = load_settings();
    UI.with(|ui| {
        ui.tray_hwnd.set(tray_hwnd);
        ui.app_icon.set(app_icon.unwrap_or_default());
        ui.taskbar_created
            .set(RegisterWindowMessageW(w!("TaskbarCreated")));
        ui.widget_visible.set(settings.visible);
        ui.widget_mode.set(settings.mode);
        ui.floating_position.set(settings.position);
        *ui.items.borrow_mut() = settings.items.clone();
    });

    if add_tray_icon(tray_hwnd) {
        TRAY_READY.store(true, Ordering::SeqCst);
    } else {
        log::warn!("no tray icon, so closing the main window will exit");
    }
    log::info!(
        "widget: {}, visible: {}, items: {:?}",
        settings.mode.token(),
        settings.visible,
        settings.items
    );
    create_widget(instance);
    refresh();
    crate::update::start(notify_update_found);
    SetTimer(tray_hwnd, TIMER_REFRESH, REFRESH_MS, None);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// The exe's embedded icon at the size closest to `size`. winresource gives it ID 1.
unsafe fn load_icon_resource(
    instance: windows::Win32::Foundation::HINSTANCE,
    size: i32,
) -> Option<HICON> {
    let handle = LoadImageW(
        instance,
        PCWSTR(1 as *const u16),
        IMAGE_ICON,
        size,
        size,
        LR_DEFAULTCOLOR,
    )
    .ok()?;
    Some(HICON(handle.0))
}

/// winit scales its one bitmap for both of the window's icons. Replacing them with
/// sizes from the icon file keeps the title bar and taskbar sharp.
unsafe fn set_main_window_icons(instance: windows::Win32::Foundation::HINSTANCE) {
    let hwnd = HWND(CONTEXT.get().unwrap().main_hwnd as *mut c_void);
    for (kind, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
        if let Some(icon) = load_icon_resource(instance, GetSystemMetrics(metric)) {
            SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(kind as usize),
                LPARAM(icon.0 as isize),
            );
        }
    }
}

/// Called from the update thread; the tray icon belongs to this one.
fn notify_update_found() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(TRAY_CLASS, PCWSTR::null()) {
            let _ = PostMessageW(hwnd, WM_UPDATE_FOUND, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe fn show_update_notification(hwnd: HWND) {
    let Some(update) = crate::update::available() else {
        return;
    };
    let mut data = notify_data(hwnd);
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_INFO;
    copy_wide(&mut data.szInfoTitle, "Monitor update available");
    copy_wide(
        &mut data.szInfo,
        &format!("Version {} is ready. Click to download it.", update.version),
    );
    let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
}

unsafe fn open_update_page() {
    if let Some(update) = crate::update::available() {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(update.url),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

/// Copies `text` into a fixed-size, null-terminated field, cutting it if needed.
fn copy_wide(field: &mut [u16], text: &str) {
    let chars = wide(text);
    let n = chars.len().min(field.len() - 1);
    field[..n].copy_from_slice(&chars[..n]);
    field[n] = 0;
}

fn set_tip(data: &mut NOTIFYICONDATAW, text: &str) {
    copy_wide(&mut data.szTip, text);
}

unsafe fn add_tray_icon(hwnd: HWND) -> bool {
    let mut data = notify_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = UI.with(|ui| ui.app_icon.get());
    set_tip(&mut data, "Monitor");
    Shell_NotifyIconW(NIM_ADD, &data).as_bool()
}

unsafe fn make_icon(w: i32, h: i32, bgra: &[[u8; 4]]) -> Option<HICON> {
    let mut color = Dib::new(w, h)?;
    color.pixels_mut().copy_from_slice(bgra);
    let mask = CreateBitmap(w, h, 1, 1, None);
    let info = ICONINFO {
        fIcon: BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color.bitmap,
    };
    // The bitmap has to be out of its DC before it can become an icon.
    SelectObject(color.dc, color.previous);
    let icon = CreateIconIndirect(&info).ok();
    color.previous = SelectObject(color.dc, color.bitmap);
    let _ = DeleteObject(mask);
    icon
}

/// The icon itself never changes; only its hover text follows the readings.
unsafe fn update_tray(tip: String) {
    let unchanged = UI.with(|ui| ui.shown_tip.borrow().as_deref() == Some(tip.as_str()));
    if unchanged {
        return;
    }
    UI.with(|ui| {
        let mut data = notify_data(ui.tray_hwnd.get());
        data.uFlags = NIF_TIP;
        set_tip(&mut data, &tip);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        *ui.shown_tip.borrow_mut() = Some(tip);
    });
}

unsafe fn refresh() {
    let readings = read_readings();
    update_tray(tooltip(
        readings.state,
        readings.cpu_load.map(|l| l.round() as i32),
    ));
    paint_widget(&readings);
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

fn show_main() {
    let context = CONTEXT.get().unwrap();
    let hwnd = HWND(context.main_hwnd as *mut c_void);
    unsafe {
        if MAIN_HIDDEN.swap(false, Ordering::SeqCst) {
            let _ = ShowWindow(hwnd, SW_HIDE);
            set_tool_window(hwnd, false);
            set_cloaked(hwnd, false);
        }
        let _ = ShowWindow(hwnd, if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
        let _ = SetForegroundWindow(hwnd);
    }
    // Back to the full redraw rate right away, rather than at the next slow tick.
    context.egui_ctx.request_repaint();
}

fn start_sensors() {
    let context = CONTEXT.get().unwrap();
    crate::enable_sensors(&context.enable_phase);
}

/// Stop the sampler, remove the tray icon, and exit.
fn quit() -> ! {
    log::info!("quit from the menu");
    let context = CONTEXT.get().unwrap();
    drop(context.sampler.lock().unwrap().take());
    UI.with(|ui| unsafe {
        // Don't leave anything behind inside Explorer's taskbar.
        let _ = DestroyWindow(ui.widget_hwnd.get());
        let _ = Shell_NotifyIconW(NIM_DELETE, &notify_data(ui.tray_hwnd.get()));
    });
    std::process::exit(0);
}

unsafe fn set_widget_visible(visible: bool) {
    log::info!("widget turned {} from the menu", if visible { "on" } else { "off" });
    UI.with(|ui| {
        ui.widget_visible.set(visible);
        let hwnd = ui.widget_hwnd.get();
        let _ = ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
    });
    if visible {
        refresh();
    }
    save_settings();
}

unsafe fn set_widget_mode(mode: WidgetMode) {
    log::info!("widget position set to {} from the menu", mode.token());
    UI.with(|ui| ui.widget_mode.set(mode));
    recreate_widget();
    save_settings();
}

/// Replaces the widget's window, needed because a floating window and a taskbar child
/// are created differently, and because a child dies with its taskbar if Explorer
/// restarts.
unsafe fn recreate_widget() {
    UI.with(|ui| {
        let _ = DestroyWindow(ui.widget_hwnd.get());
        ui.widget_hwnd.set(HWND::default());
        *ui.shown_widget.borrow_mut() = None;
    });
    if let Ok(module) = GetModuleHandleW(None) {
        create_widget(module.into());
    }
    refresh();
}

/// The checklist of where the widget can live.
unsafe fn modes_menu() -> Option<HMENU> {
    let menu = CreatePopupMenu().ok()?;
    let current = UI.with(|ui| ui.widget_mode.get());
    for (index, mode) in WIDGET_MODES.into_iter().enumerate() {
        let check = if mode == current { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(menu, MF_STRING | check, MENU_MODE_BASE + index, mode.menu_label());
    }
    Some(menu)
}

/// The checklist of readings the widget can show.
unsafe fn items_menu(catalog: &[WidgetItem], gpu_count: usize) -> Option<HMENU> {
    let menu = CreatePopupMenu().ok()?;
    let selected = UI.with(|ui| ui.items.borrow().clone());
    let mut group = None;
    for (index, &item) in catalog.iter().enumerate() {
        if group.is_some_and(|g| g != item.group()) {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        group = Some(item.group());
        let checked = selected.contains(&item);
        let mut flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
        if checked && selected.len() == 1 {
            flags |= MF_GRAYED; // the last one can't be switched off
        }
        let label = HSTRING::from(item.menu_label(gpu_count));
        let _ = AppendMenuW(menu, flags, MENU_ITEM_BASE + index, &label);
    }
    Some(menu)
}

unsafe fn toggle_item(item: WidgetItem) {
    log::info!("widget item toggled: {}", item.token());
    UI.with(|ui| {
        let next = toggled(&ui.items.borrow(), item);
        *ui.items.borrow_mut() = next;
    });
    refresh();
    save_settings();
}

unsafe fn show_menu() {
    let readings = read_readings();
    let state = readings.state;
    let gpu_count = readings.gpus.len();
    let catalog = all_items(gpu_count);
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let visible = UI.with(|ui| ui.widget_visible.get());
    let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("&Open Monitor"));
    if let Some(update) = crate::update::available() {
        let label = HSTRING::from(format!("&Download update {}", update.version));
        let _ = AppendMenuW(menu, MF_STRING, MENU_UPDATE, &label);
    }
    let check = if visible { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(menu, MF_STRING | check, MENU_WIDGET, w!("Show &widget"));
    if let Some(items) = items_menu(&catalog, gpu_count) {
        let _ = AppendMenuW(menu, MF_POPUP, items.0 as usize, w!("Widget &items"));
    }
    if let Some(modes) = modes_menu() {
        let _ = AppendMenuW(menu, MF_POPUP, modes.0 as usize, w!("Widget &position"));
    }
    if state == TempState::Off {
        let _ = AppendMenuW(menu, MF_STRING, MENU_ENABLE, w!("&Enable CPU temperature"));
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("&Quit"));

    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let owner = UI.with(|ui| ui.tray_hwnd.get());
    // Without foreground and the follow-up message, the menu wouldn't close when
    // clicking elsewhere (documented TrackPopupMenu behaviour).
    let _ = SetForegroundWindow(owner);
    let chosen = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        cursor.x,
        cursor.y,
        0,
        owner,
        None,
    );
    let _ = PostMessageW(owner, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    match chosen.0 as usize {
        MENU_OPEN => show_main(),
        MENU_UPDATE => open_update_page(),
        MENU_WIDGET => set_widget_visible(!visible),
        MENU_ENABLE => start_sensors(),
        MENU_QUIT => quit(),
        id if (MENU_MODE_BASE..MENU_MODE_BASE + WIDGET_MODES.len()).contains(&id) => {
            set_widget_mode(WIDGET_MODES[id - MENU_MODE_BASE]);
        }
        id if id >= MENU_ITEM_BASE => {
            if let Some(&item) = catalog.get(id - MENU_ITEM_BASE) {
                toggle_item(item);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => show_main(),
                WM_RBUTTONUP => show_menu(),
                NIN_BALLOONUSERCLICK => open_update_page(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_SHOW_MAIN => {
            show_main();
            LRESULT(0)
        }
        WM_QUIT_APP => quit(),
        WM_UPDATE_FOUND => {
            show_update_notification(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            refresh();
            LRESULT(0)
        }
        _ if msg != 0 && UI.with(|ui| ui.taskbar_created.get()) == msg => {
            // Explorer restarted and lost every tray icon, and any embedded widget.
            log::info!("Explorer restarted, restoring the tray icon and widget");
            if add_tray_icon(hwnd) {
                TRAY_READY.store(true, Ordering::SeqCst);
            }
            UI.with(|ui| *ui.shown_tip.borrow_mut() = None);
            recreate_widget();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// Widget
// ---------------------------------------------------------------------------

unsafe fn widget_scale(hwnd: HWND) -> f32 {
    GetDpiForWindow(hwnd).max(96) as f32 / 96.0
}

fn widget_height(rows: usize, scale: f32) -> i32 {
    ((WIDGET_PAD_Y * 2.0 + WIDGET_ROW_H * rows as f32) * scale).round() as i32
}

unsafe fn default_widget_position(scale: f32) -> (i32, i32) {
    let mut work = RECT::default();
    let _ = SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work as *mut RECT as *mut c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    let width = (WIDGET_W * scale) as i32;
    (work.right - width - WIDGET_MARGIN, work.top + WIDGET_MARGIN)
}

/// The taskbar's window, its screen rectangle, and where the tray area starts.
/// `None` if it can't be found, or isn't a horizontal bar along the screen.
unsafe fn find_taskbar() -> Option<(HWND, RECT, i32)> {
    let taskbar = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).ok()?;
    let mut rect = RECT::default();
    GetWindowRect(taskbar, &mut rect).ok()?;
    if rect.right - rect.left <= rect.bottom - rect.top {
        return None;
    }
    let tray = FindWindowExW(taskbar, None, w!("TrayNotifyWnd"), PCWSTR::null()).ok()?;
    let mut tray_rect = RECT::default();
    GetWindowRect(tray, &mut tray_rect).ok()?;
    Some((taskbar, rect, tray_rect.left))
}

unsafe fn create_widget(instance: windows::Win32::Foundation::HINSTANCE) {
    let mode = UI.with(|ui| ui.widget_mode.get());
    let created = match mode {
        // A layered child window needs Windows 8 or later declared in the manifest,
        // which embed-manifest's default does.
        WidgetMode::TaskbarEmbedded => find_taskbar().and_then(|(taskbar, ..)| {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                WIDGET_CLASS,
                w!("Monitor widget"),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                WIDGET_W as i32,
                widget_height(1, 1.0),
                taskbar,
                None,
                instance,
                None,
            )
            .ok()
        }),
        _ => CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            WIDGET_CLASS,
            w!("Monitor widget"),
            WS_POPUP,
            0,
            0,
            WIDGET_W as i32,
            widget_height(1, 1.0),
            // The taskbar rises whenever it takes focus. Windows always keeps owned
            // windows above their owner, so this keeps the overlay visible with no
            // fighting over the z-order.
            if mode == WidgetMode::TaskbarOverlay {
                find_taskbar().map_or(HWND::default(), |(taskbar, ..)| taskbar)
            } else {
                HWND::default()
            },
            None,
            instance,
            None,
        )
        .ok(),
    };
    let Some(hwnd) = created else {
        // Embedding is the fragile one: fall back rather than show no widget at all.
        log::warn!(
            "couldn't create the {} widget: {}",
            mode.token(),
            windows::core::Error::from_win32().message()
        );
        if mode != WidgetMode::Floating {
            UI.with(|ui| ui.widget_mode.set(WidgetMode::Floating));
            create_widget(instance);
        }
        return;
    };

    if mode == WidgetMode::Floating {
        let scale = widget_scale(hwnd);
        // A saved position is only used if the widget's whole width still fits on one
        // connected monitor.
        let monitor_at = |x: i32, y: i32| MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL);
        let width = (WIDGET_W * scale) as i32;
        let (x, y) = UI
            .with(|ui| ui.floating_position.get())
            .filter(|&(x, y)| {
                let left = monitor_at(x, y);
                !left.is_invalid() && left == monitor_at(x + width - 1, y)
            })
            .unwrap_or_else(|| default_widget_position(scale));
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
    }

    UI.with(|ui| {
        ui.widget_hwnd.set(hwnd);
        if ui.widget_visible.get() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    });
}

/// Windows' own "don't disturb" states, when an overlay has no business being on top.
unsafe fn fullscreen_app_running() -> bool {
    matches!(
        SHQueryUserNotificationState(),
        Ok(state) if state == QUNS_BUSY
            || state == QUNS_RUNNING_D3D_FULL_SCREEN
            || state == QUNS_PRESENTATION_MODE
    )
}

/// An auto-hiding taskbar slides off the bottom of the screen; the overlay should too.
fn taskbar_on_screen(taskbar_top: i32, screen_height: i32) -> bool {
    taskbar_top < screen_height - SCREEN_EDGE_TOLERANCE
}

/// Keeps a taskbar widget just left of the tray, and an overlay out of the way of
/// full-screen apps. Runs on every refresh, because the tray changes width.
unsafe fn place_on_taskbar(hwnd: HWND, mode: WidgetMode) {
    let visible = UI.with(|ui| ui.widget_visible.get());
    let Some((_, bar, tray_left)) = find_taskbar() else {
        return;
    };
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let x = tray_left - w - TASKBAR_TRAY_GAP;
    let y = bar.top + (bar.bottom - bar.top - h) / 2;

    if mode == WidgetMode::TaskbarEmbedded {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOP,
            x - bar.left,
            y - bar.top,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
        return;
    }

    let fullscreen = fullscreen_app_running();
    let bar_on_screen = taskbar_on_screen(bar.top, GetSystemMetrics(SM_CYSCREEN));
    let show = visible && !fullscreen && bar_on_screen;
    if show != IsWindowVisible(hwnd).as_bool() {
        log::info!(
            "overlay {}: widget on: {visible}, full-screen app: {fullscreen}, taskbar on screen: {bar_on_screen}",
            if show { "shown" } else { "hidden" }
        );
        let _ = ShowWindow(hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
    if show && (rect.left, rect.top) != (x, y) {
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Whether the taskbar uses the light theme, so the text can contrast with it.
unsafe fn taskbar_is_light() -> bool {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = RegGetValueW(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("SystemUsesLightTheme"),
        RRF_RT_REG_DWORD,
        None,
        Some(&mut value as *mut u32 as *mut c_void),
        Some(&mut size),
    );
    result.is_ok() && value == 1
}

unsafe fn paint_widget(readings: &Readings) {
    let (hwnd, visible, mode) =
        UI.with(|ui| (ui.widget_hwnd.get(), ui.widget_visible.get(), ui.widget_mode.get()));
    if hwnd.is_invalid() {
        return;
    }
    if visible {
        let rows = UI.with(|ui| widget_rows(&ui.items.borrow(), readings));
        let key = WidgetKey {
            rows,
            dpi: GetDpiForWindow(hwnd),
            mode,
            light_taskbar: mode.on_taskbar() && taskbar_is_light(),
            height: find_taskbar().map_or(0, |(_, bar, _)| bar.bottom - bar.top),
        };
        let unchanged = UI.with(|ui| ui.shown_widget.borrow().as_ref() == Some(&key));
        if !unchanged {
            let scale = widget_scale(hwnd);
            let canvas = if mode.on_taskbar() {
                render_taskbar_widget(&key.rows, scale, key.height, key.light_taskbar)
            } else {
                render_widget(&key.rows, scale)
            };
            present(hwnd, &canvas, mode);
            UI.with(|ui| *ui.shown_widget.borrow_mut() = Some(key));
        }
    }
    if mode.on_taskbar() {
        place_on_taskbar(hwnd, mode);
    }
}

/// Rows in pairs, as columns of "label value", with no panel behind them, so the
/// widget looks like part of the taskbar.
unsafe fn render_taskbar_widget(rows: &[Row], scale: f32, height: i32, light: bool) -> Canvas {
    let px = |v: f32| (v * scale).round() as i32;
    let text_px = px(TASKBAR_TEXT_PX);
    let columns: Vec<&[Row]> = rows.chunks(TASKBAR_ROWS_PER_COLUMN).collect();
    let widths: Vec<(i32, i32)> = columns
        .iter()
        .map(|column| {
            let label = column
                .iter()
                .map(|r| text_width(&r.label, text_px, false))
                .max()
                .unwrap_or(0);
            let value = column
                .iter()
                .map(|r| text_width(&r.value, text_px, true))
                .max()
                .unwrap_or(0);
            (label, value)
        })
        .collect();
    let width = taskbar_widget_width(
        &widths,
        px(TASKBAR_LABEL_GAP),
        px(TASKBAR_COLUMN_GAP),
        px(TASKBAR_PAD_X),
    );

    let mut canvas = Canvas::new(width, height.max(1));
    for y in 0..canvas.h {
        for x in 0..canvas.w {
            canvas.blend(x, y, TASKBAR_HIT_AREA, 1.0);
        }
    }
    let label_color = if light { TASKBAR_LIGHT_DIM } else { TEXT_DIM };
    let row_h = px(TASKBAR_ROW_H);
    let mut x = px(TASKBAR_PAD_X);
    for (column, (label_w, value_w)) in columns.iter().zip(&widths) {
        let top = (canvas.h - row_h * column.len() as i32) / 2;
        let right = x + label_w + px(TASKBAR_LABEL_GAP) + value_w;
        for (i, row) in column.iter().enumerate() {
            let y = top + row_h * i as i32;
            let value_color = if light && row.color == TEXT_MAIN {
                TASKBAR_LIGHT_TEXT
            } else {
                row.color
            };
            let label = TextStyle {
                px: text_px,
                bold: false,
                color: label_color,
            };
            let value = TextStyle {
                px: text_px,
                bold: true,
                color: value_color,
            };
            draw_text(&mut canvas, &row.label, x, y, Align::Left, &label);
            draw_text(&mut canvas, &row.value, right, y, Align::Right, &value);
        }
        x = right + px(TASKBAR_COLUMN_GAP);
    }
    canvas
}

fn taskbar_widget_width(columns: &[(i32, i32)], label_gap: i32, column_gap: i32, pad: i32) -> i32 {
    let content: i32 = columns.iter().map(|(l, v)| l + label_gap + v).sum();
    let gaps = column_gap * (columns.len() as i32 - 1).max(0);
    content + gaps + pad * 2
}

unsafe fn render_widget(rows: &[Row], scale: f32) -> Canvas {
    let px = |v: f32| (v * scale).round() as i32;
    let mut canvas = Canvas::new(px(WIDGET_W), widget_height(rows.len(), scale));
    canvas.fill_panel(WIDGET_RADIUS * scale, PANEL, PANEL_EDGE, scale.max(1.0));

    let right = canvas.w - px(WIDGET_PAD_X);
    for (i, row) in rows.iter().enumerate() {
        let y = px(WIDGET_PAD_Y + WIDGET_ROW_H * i as f32) + px(1.0);
        let label = TextStyle {
            px: px(WIDGET_TEXT_PX),
            bold: false,
            color: TEXT_DIM,
        };
        let value = TextStyle {
            px: px(WIDGET_TEXT_PX),
            bold: true,
            color: row.color,
        };
        draw_text(&mut canvas, &row.label, px(WIDGET_PAD_X), y, Align::Left, &label);
        draw_text(&mut canvas, &row.value, right, y, Align::Right, &value);
    }
    canvas
}

unsafe fn present(hwnd: HWND, canvas: &Canvas, mode: WidgetMode) {
    let Some(mut dib) = Dib::new(canvas.w, canvas.h) else {
        return;
    };
    dib.pixels_mut().copy_from_slice(&canvas.px);
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let position = POINT {
        x: rect.left,
        y: rect.top,
    };
    // Only top-level windows take a screen position here; a child is moved separately.
    let destination = (mode != WidgetMode::TaskbarEmbedded).then_some(&position as *const POINT);
    let size = SIZE {
        cx: canvas.w,
        cy: canvas.h,
    };
    let source = POINT::default();
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let screen = GetDC(None);
    let _ = UpdateLayeredWindow(
        hwnd,
        screen,
        destination,
        Some(&size),
        dib.dc,
        Some(&source),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
    ReleaseDC(None, screen);
}

unsafe fn widget_clicked() {
    log::info!("widget clicked");
    if should_start_sensors_on_click() {
        start_sensors();
        refresh();
    } else {
        show_main();
    }
}

/// A click starts the sensors only if the widget is showing a reading that needs
/// them; otherwise a surprise admin prompt would be the last thing expected.
fn should_start_sensors_on_click() -> bool {
    let shows_sensor_item = UI.with(|ui| ui.items.borrow().iter().any(|i| i.needs_sensors()));
    shows_sensor_item && read_readings().state == TempState::Off
}

unsafe extern "system" fn widget_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN if UI.with(|ui| ui.widget_mode.get()).on_taskbar() => {
            // Taskbar positions are automatic, so there's nothing to drag.
            widget_clicked();
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Let Windows run the drag. It returns on release, and a window that
            // didn't move means this was a click.
            let mut before = RECT::default();
            let _ = GetWindowRect(hwnd, &mut before);
            let _ = ReleaseCapture();
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDOWN,
                WPARAM(HTCAPTION as usize),
                LPARAM(0),
            );
            let mut after = RECT::default();
            let _ = GetWindowRect(hwnd, &mut after);
            if (before.left, before.top) != (after.left, after.top) {
                UI.with(|ui| ui.floating_position.set(Some((after.left, after.top))));
                save_settings();
            } else {
                widget_clicked();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_menu();
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DESTROY => {
            log::info!("widget window destroyed");
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let suggested = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                None,
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            refresh();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Settings {
    position: Option<(i32, i32)>,
    visible: bool,
    items: Vec<WidgetItem>,
    mode: WidgetMode,
}

fn settings_path() -> Option<PathBuf> {
    Some(logging::app_data_dir()?.join("widget.txt"))
}

/// Parses "x y visible item,item,... mode"; anything unreadable falls back to the
/// defaults. x and y are the floating position, kept while the widget is on the taskbar.
fn parse_settings(text: &str) -> Settings {
    let mut parts = text.split_whitespace();
    let x = parts.next().and_then(|v| v.parse().ok());
    let y = parts.next().and_then(|v| v.parse().ok());
    let visible = parts.next().and_then(|v| v.parse().ok()).unwrap_or(true);
    let mut items: Vec<WidgetItem> = parts
        .next()
        .unwrap_or_default()
        .split(',')
        .filter_map(WidgetItem::from_token)
        .collect();
    items.sort();
    items.dedup();
    if items.is_empty() {
        items = default_items();
    }
    let mode = parts
        .next()
        .and_then(WidgetMode::from_token)
        .unwrap_or(DEFAULT_WIDGET_MODE);
    Settings {
        position: x.zip(y),
        visible,
        items,
        mode,
    }
}

fn load_settings() -> Settings {
    let text = settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    parse_settings(&text)
}

unsafe fn save_settings() {
    let Some(path) = settings_path() else {
        return;
    };
    let (position, mode, visible, items) = UI.with(|ui| {
        (
            ui.floating_position.get(),
            ui.widget_mode.get(),
            ui.widget_visible.get(),
            ui.items.borrow().iter().map(|i| i.token()).collect::<Vec<_>>().join(","),
        )
    });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let (x, y) = position.map_or(("-".to_string(), "-".to_string()), |(x, y)| {
        (x.to_string(), y.to_string())
    });
    let _ = std::fs::write(
        path,
        format!("{x} {y} {visible} {items} {}", mode.token()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_wins_over_every_other_state() {
        assert_eq!(
            temp_state(Some(47.4), false, EnablePhase::Idle),
            TempState::Live(47)
        );
    }

    #[test]
    fn missing_temperature_offers_the_service_only_when_it_could_help() {
        let state = |alive, phase| temp_state(None, alive, phase);
        assert_eq!(state(false, EnablePhase::Idle), TempState::Off);
        assert_eq!(state(false, EnablePhase::Installing), TempState::Starting);
        // Service running without a reading: a click can't help.
        assert_eq!(state(true, EnablePhase::Idle), TempState::Unavailable);
    }

    #[test]
    fn tooltip_includes_temperature_and_load() {
        assert_eq!(tooltip(TempState::Live(47), Some(12)), "CPU 47 °C · Load 12%");
        assert_eq!(tooltip(TempState::Off, None), "CPU temperature off");
    }

    #[test]
    fn settings_round_trip_and_tolerate_bad_files() {
        assert_eq!(
            parse_settings("100 -20 false"),
            Settings {
                position: Some((100, -20)),
                visible: false,
                items: default_items(),
                mode: DEFAULT_WIDGET_MODE,
            }
        );
        let defaults = Settings {
            position: None,
            visible: true,
            items: default_items(),
            mode: DEFAULT_WIDGET_MODE,
        };
        assert_eq!(parse_settings(""), defaults);
        assert_eq!(parse_settings("not numbers"), defaults);
    }

    #[test]
    fn panel_is_translucent_inside_and_clear_past_the_rounded_corner() {
        let mut c = Canvas::new(40, 40);
        c.fill_panel(10.0, Rgba(255, 0, 0, 128), Rgba(0, 0, 0, 0), 0.0);
        let at = |x: i32, y: i32| c.px[(y * c.w + x) as usize];
        assert_eq!(at(0, 0), [0, 0, 0, 0], "corner should be fully transparent");
        let center = at(20, 20);
        assert_eq!(center[3], 128, "center should be half transparent");
        assert_eq!(center[2], 128, "red should be premultiplied by alpha");
    }

    #[test]
    fn a_fresh_install_gets_the_taskbar_widget_with_the_main_readings() {
        let s = parse_settings("");
        assert_eq!(s.mode, WidgetMode::TaskbarOverlay);
        assert!(s.visible);
        assert_eq!(
            s.items,
            vec![
                WidgetItem::CpuTemp,
                WidgetItem::CpuLoad,
                WidgetItem::CpuPower,
                WidgetItem::Memory,
            ]
        );
    }

    #[test]
    fn widget_mode_round_trips_and_survives_a_missing_position() {
        for mode in WIDGET_MODES {
            assert_eq!(WidgetMode::from_token(mode.token()), Some(mode));
        }
        let s = parse_settings("- - true cpu_temp embedded");
        assert_eq!(s.position, None);
        assert_eq!(s.mode, WidgetMode::TaskbarEmbedded);
        assert_eq!(s.items, vec![WidgetItem::CpuTemp]);
        assert_eq!(parse_settings("1 2 true cpu_temp nonsense").mode, DEFAULT_WIDGET_MODE);
        assert_eq!(parse_settings("1 2 true cpu_temp floating").mode, WidgetMode::Floating);
    }

    #[test]
    fn taskbar_width_adds_columns_gaps_and_padding() {
        assert_eq!(taskbar_widget_width(&[(50, 30)], 6, 14, 6), 50 + 6 + 30 + 12);
        assert_eq!(
            taskbar_widget_width(&[(50, 30), (40, 20)], 6, 14, 6),
            (50 + 6 + 30) + 14 + (40 + 6 + 20) + 12
        );
        assert_eq!(taskbar_widget_width(&[], 6, 14, 6), 12);
    }

    #[test]
    fn overlay_only_shows_while_the_taskbar_is_on_screen() {
        assert!(taskbar_on_screen(1392, 1440));
        assert!(!taskbar_on_screen(1438, 1440), "auto-hidden taskbar");
    }

    #[test]
    fn every_item_round_trips_through_its_settings_token() {
        for item in all_items(2) {
            assert_eq!(WidgetItem::from_token(&item.token()), Some(item));
        }
        assert_eq!(WidgetItem::from_token("nonsense"), None);
        assert_eq!(WidgetItem::from_token("gpu_x_temp"), None);
        assert_eq!(WidgetItem::from_token("gpu0_nonsense"), None);
    }

    #[test]
    fn saved_items_load_in_display_order_and_skip_unknown_ones() {
        let s = parse_settings("1 2 true memory,bogus,cpu_load,gpu1_vram,cpu_load");
        assert_eq!(
            s.items,
            vec![
                WidgetItem::CpuLoad,
                WidgetItem::Gpu(1, GpuMetric::Vram),
                WidgetItem::Memory
            ]
        );
    }

    #[test]
    fn toggling_adds_in_order_removes_and_keeps_the_last_item() {
        let items = toggled(&[WidgetItem::CpuTemp, WidgetItem::Memory], WidgetItem::CpuLoad);
        assert_eq!(
            items,
            vec![WidgetItem::CpuTemp, WidgetItem::CpuLoad, WidgetItem::Memory]
        );
        let items = toggled(&items, WidgetItem::CpuLoad);
        assert_eq!(items, vec![WidgetItem::CpuTemp, WidgetItem::Memory]);
        let last = [WidgetItem::Memory];
        assert_eq!(toggled(&last, WidgetItem::Memory), last);
    }

    fn sample_readings(state: TempState) -> Readings {
        Readings {
            state,
            cpu_load: Some(12.4),
            cpu_mhz: Some(4500.0),
            cpu_power: None,
            gpus: vec![GpuState {
                temp: Some(36.6),
                mem_used_mb: Some(2048.0),
                mem_total_mb: Some(16384.0),
                ..Default::default()
            }],
            mem_used_gb: 14.84,
            mem_total_gb: 29.8,
        }
    }

    fn values(items: &[WidgetItem], readings: &Readings) -> Vec<(String, String)> {
        widget_rows(items, readings)
            .into_iter()
            .map(|r| (r.label, r.value))
            .collect()
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn rows_format_each_kind_of_reading() {
        let items = [
            WidgetItem::CpuTemp,
            WidgetItem::CpuLoad,
            WidgetItem::CpuFreq,
            WidgetItem::Gpu(0, GpuMetric::Temp),
            WidgetItem::Gpu(0, GpuMetric::Vram),
            WidgetItem::Memory,
        ];
        assert_eq!(
            values(&items, &sample_readings(TempState::Live(48))),
            vec![
                pair("CPU temp", "48 °C"),
                pair("CPU load", "12%"),
                pair("CPU freq", "4.50 GHz"),
                pair("GPU temp", "37 °C"),
                pair("GPU VRAM", "2.0 / 16 GB"),
                pair("Memory", "14.8 / 30 GB"),
            ]
        );
    }

    #[test]
    fn only_the_first_sensor_row_shows_the_enable_hint() {
        let items = [WidgetItem::CpuTemp, WidgetItem::CpuLoad, WidgetItem::CpuPower];
        assert_eq!(
            values(&items, &sample_readings(TempState::Off)),
            vec![
                pair("CPU temp", "Click to enable"),
                pair("CPU load", "12%"),
                pair("CPU power", NO_VALUE),
            ]
        );
    }

    #[test]
    fn a_missing_gpu_is_skipped_and_the_widget_never_ends_up_empty() {
        let readings = sample_readings(TempState::Live(48));
        let gone = [WidgetItem::Gpu(3, GpuMetric::Temp)];
        assert_eq!(values(&gone, &readings), vec![pair("Monitor", NO_VALUE)]);
    }

    #[test]
    fn gpu_rows_are_numbered_only_with_several_gpus() {
        let item = WidgetItem::Gpu(1, GpuMetric::Load);
        assert_eq!(item.row_label(2), "GPU 2 load");
        assert_eq!(WidgetItem::Gpu(0, GpuMetric::Load).row_label(1), "GPU load");
    }
}

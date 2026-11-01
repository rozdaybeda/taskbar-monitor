#![windows_subsystem = "windows"]

mod logging;
mod sensors;
mod settings;
#[allow(dead_code)] // some codes are only used by the setup crate
mod setup_exit;
mod tray;
mod update;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

use sensors::bench::{BenchKind, BenchRunner, BenchSlot, BenchStatus};
use sensors::Sampler;

const HISTORY: usize = 120; // 2 minutes at one point per second
/// Height one extra GPU card adds to the window, including its gap.
const GPU_CARD_HEIGHT: f32 = 190.0;
const GPU_ACCENTS: [egui::Color32; 2] = [
    egui::Color32::from_rgb(120, 220, 140),
    egui::Color32::from_rgb(240, 150, 90),
];

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Used by the installer, so it can replace the exe of a running copy.
    if args.iter().any(|a| a == "--quit") {
        tray::quit_running_instance();
        return Ok(());
    }
    // Used by the installer when uninstalling.
    if args.iter().any(|a| a == "--remove-autostart") {
        settings::remove_own_auto_start();
        return Ok(());
    }
    // Run as the Windows service, with no GUI and no single-instance guard.
    if args.iter().any(|a| a == "--service") {
        if let Some(folder) = sensors::service::logs_folder() {
            logging::init_in(&folder, logging::SERVICE_LOG);
        }
        sensors::service::run_service();
        return Ok(());
    }
    // Single-instance guard (GUI only): a named mutex. If it already exists,
    // another copy is running, so exit quietly. The handle is intentionally
    // leaked so the mutex lives for the whole process.
    #[cfg(windows)]
    {
        use windows::core::w;
        use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;
        unsafe {
            let _ = CreateMutexW(None, false, w!("Local\\Monitor_SingleInstance_v1"));
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // It may be running hidden in the tray, so bring it back.
                tray::signal_running_instance();
                return Ok(());
            }
        }
    }

    // After the guard, so a launch that exits right away doesn't leave a log file.
    logging::init(logging::GUI_LOG);

    log::info!(
        "Monitor {} started, pid {}, elevated: {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        is_elevated()
    );
    settings::init();
    let shared = Arc::new(Mutex::new(Shared::default()));
    let sampler = Sampler::start(shared.clone());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 720.0])
            .with_min_inner_size([340.0, 520.0])
            .with_title("Monitor")
            .with_icon(Arc::new(app_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "Monitor",
        options,
        Box::new(move |cc| Ok(Box::new(MonitorApp::new(cc, shared, sampler)))),
    )
}

/// Is the current process running elevated (as administrator)?
#[cfg(windows)]
fn is_elevated() -> bool {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
fn is_elevated() -> bool {
    false
}

/// Load the window / taskbar icon from the baked-in raw RGBA bytes.
fn app_icon() -> egui::IconData {
    let rgba = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon_rgba.bin")).to_vec();
    egui::IconData {
        rgba,
        width: 64,
        height: 64,
    }
}

// ---------------------------------------------------------------------------
// Shared state: the sampler thread (see `sensors`) writes readings here; the
// UI snapshots it each frame.
// ---------------------------------------------------------------------------

/// Latest readings for one GPU. Values stay `None` if the card can't report them.
#[derive(Clone, Default)]
struct GpuState {
    name: String,
    load_source: &'static str,
    temp: Option<f32>,
    clock: Option<f32>,
    load: Option<f32>,
    power: Option<f32>,
    mem_used_mb: Option<f32>,
    mem_total_mb: Option<f32>,
}

#[derive(Default)]
struct Shared {
    device_info: Option<sensors::DeviceInfo>,
    status: String,
    cpu_name: String,
    cpu_temp: Option<f32>,
    cpu_clock: Option<f32>,
    cpu_load: Option<f32>,
    cpu_power: Option<f32>,
    cpu_cores: Vec<f32>,
    cpu_core_clocks: Vec<f32>,
    sensor_service_alive: bool,
    sensor_failure: Option<String>,
    gpus: Vec<GpuState>,
    mem_used_pct: Option<f32>,
    mem_used_gb: f32,
    mem_total_gb: f32,
    sensors: Vec<(String, f32)>,
    cpu_hist: Series,
    mem_hist: Series,
    gpu_hists: Vec<Series>,
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

/// Rolling load history, one point per second. `None` means no reading was
/// available for that second, and is drawn as a gap rather than as 0%.
#[derive(Clone, Default)]
struct Series {
    data: VecDeque<Option<f32>>,
}

impl Series {
    fn push(&mut self, v: Option<f32>) {
        if self.data.len() == HISTORY {
            self.data.pop_front();
        }
        self.data.push_back(v);
    }

    /// Stretches of consecutive readings, as (position in history, value).
    fn runs(&self) -> Vec<Vec<(usize, f32)>> {
        let mut runs: Vec<Vec<(usize, f32)>> = Vec::new();
        let mut in_run = false;
        for (i, v) in self.data.iter().enumerate() {
            match v {
                Some(v) if in_run => runs.last_mut().unwrap().push((i, *v)),
                Some(v) => {
                    runs.push(vec![(i, *v)]);
                    in_run = true;
                }
                None => in_run = false,
            }
        }
        runs
    }
}

struct MonitorApp {
    shared: Arc<Mutex<Shared>>,
    // Dropping the sampler stops its thread and unloads the kernel driver. It's
    // shared because the tray's Quit has to do that too.
    sampler: Arc<Mutex<Option<Sampler>>>,
    main_hwnd: Option<isize>,
    show_info: bool,
    info_tab: usize,
    window_fitted: bool,
    bench: BenchRunner,
    enable_phase: Arc<Mutex<EnablePhase>>,
}

/// Progress of turning the CPU sensors on, shown as a spinner on the TEMP stat.
#[derive(Clone, Copy, PartialEq)]
enum EnablePhase {
    Idle,
    /// The admin prompt is open, or the service is being installed.
    Installing,
    /// Installed; waiting for the service's first reading.
    Starting(Instant),
}

/// How long to wait for a just-installed service before offering to try again.
const SERVICE_START_GRACE: Duration = Duration::from_secs(10);

/// Current phase, cleared once the service is up or has timed out.
fn current_enable_phase(phase: &Mutex<EnablePhase>, service_alive: bool) -> EnablePhase {
    let mut phase = phase.lock().unwrap();
    let expired = matches!(*phase, EnablePhase::Starting(t) if t.elapsed() >= SERVICE_START_GRACE);
    if expired || (service_alive && *phase != EnablePhase::Installing) {
        *phase = EnablePhase::Idle;
    }
    *phase
}

/// Install the sensor service on a background thread, so nothing freezes while the admin
/// prompt is open. Does nothing if that's already in progress.
fn enable_sensors(phase: &Arc<Mutex<EnablePhase>>) {
    {
        let mut current = phase.lock().unwrap();
        if *current != EnablePhase::Idle {
            return;
        }
        *current = EnablePhase::Installing;
    }
    sensors::set_setup_failure(None);
    log::info!("installing the sensor service, showing the admin prompt");
    let phase = phase.clone();
    std::thread::spawn(move || {
        let next = match sensors::setup_client::request_install() {
            Ok(true) => EnablePhase::Starting(Instant::now()),
            Ok(false) => {
                log::info!("the admin prompt was declined");
                EnablePhase::Idle
            }
            Err(failure) => {
                log::warn!("{failure}");
                sensors::set_setup_failure(Some(failure));
                EnablePhase::Idle
            }
        };
        *phase.lock().unwrap() = next;
    });
}

/// The window's native handle, if it's a Win32 window.
fn native_hwnd(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

impl MonitorApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        shared: Arc<Mutex<Shared>>,
        sampler: Sampler,
    ) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(20, 22, 26);
        visuals.window_fill = egui::Color32::from_rgb(20, 22, 26);
        cc.egui_ctx.set_visuals(visuals);

        let bench_shared = shared.clone();
        let sampler = Arc::new(Mutex::new(Some(sampler)));
        let enable_phase = Arc::new(Mutex::new(EnablePhase::Idle));
        let main_hwnd = native_hwnd(cc);
        if let Some(main_hwnd) = main_hwnd {
            tray::start(tray::TrayContext {
                shared: shared.clone(),
                egui_ctx: cc.egui_ctx.clone(),
                main_hwnd,
                sampler: sampler.clone(),
                enable_phase: enable_phase.clone(),
            });
        }

        Self {
            shared,
            sampler,
            main_hwnd,
            show_info: false,
            info_tab: 0,
            window_fitted: false,
            bench: BenchRunner::new(bench_shared),
            enable_phase,
        }
    }
}

impl eframe::App for MonitorApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        log::info!("exiting");
        drop(self.sampler.lock().unwrap().take());
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The window's close button sends the app to the tray, along with any open
        // Device Info window.
        if ctx.input(|i| i.viewport().close_requested()) && tray::is_ready() {
            if let Some(hwnd) = self.main_hwnd {
                log::info!("main window closed, sending it to the tray");
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.show_info = false;
                tray::hide_main_window(hwnd);
            }
        }

        // Snapshot the shared state once per frame.
        let snap = {
            let s = self.shared.lock().unwrap();
            Snapshot {
                status: s.status.clone(),
                cpu_name: s.cpu_name.clone(),
                cpu_temp: s.cpu_temp,
                cpu_clock: s.cpu_clock,
                cpu_load: s.cpu_load,
                cpu_power: s.cpu_power,
                cpu_cores: s.cpu_cores.clone(),
                cpu_core_clocks: s.cpu_core_clocks.clone(),
                sensor_service_alive: s.sensor_service_alive,
                sensor_failure: s.sensor_failure.clone(),
                gpus: s.gpus.clone(),
                mem_used_pct: s.mem_used_pct,
                mem_used_gb: s.mem_used_gb,
                mem_total_gb: s.mem_total_gb,
                sensors: s.sensors.clone(),
                cpu_hist: s.cpu_hist.clone(),
                mem_hist: s.mem_hist.clone(),
                gpu_hists: s.gpu_hists.clone(),
            }
        };

        // The window starts sized for one GPU; grow it once for any extras.
        if !self.window_fitted && !snap.gpus.is_empty() {
            self.window_fitted = true;
            let extra = snap.gpus.len() - 1;
            if extra > 0 {
                let screen = ctx.screen_rect().size();
                let wanted = screen.y + GPU_CARD_HEIGHT * extra as f32;
                let limit = ctx
                    .input(|i| i.viewport().monitor_size)
                    .map_or(wanted, |m| m.y - 80.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    screen.x,
                    wanted.min(limit).max(screen.y),
                )));
            }
        }

        // Status line pinned to the bottom, with a painted status dot.
        egui::TopBottomPanel::bottom("status")
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .inner_margin(egui::Margin {
                        left: 14.0,
                        right: 14.0,
                        top: 4.0,
                        bottom: 8.0,
                    })
                    .fill(egui::Color32::from_rgb(20, 22, 26)),
            )
            .show(ctx, |ui| {
                let color = if snap.status == "live" {
                    egui::Color32::from_rgb(120, 220, 140)
                } else {
                    egui::Color32::from_rgb(220, 160, 90)
                };
                ui.horizontal(|ui| {
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, color);
                    ui.label(egui::RichText::new(&snap.status).size(11.0).color(color));
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .inner_margin(14.0)
                    .fill(egui::Color32::from_rgb(20, 22, 26)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(egui::RichText::new("System Monitor").size(20.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Info").clicked() {
                            self.show_info = !self.show_info;
                        }
                    });
                });
                ui.add_space(10.0);

                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        // CPU card
                        let cpu_sub = if snap.cpu_name.is_empty() {
                            None
                        } else {
                            Some(snap.cpu_name.as_str())
                        };
                        // With no sensor service running, TEMP becomes a restart icon
                        // that installs it.
                        let cpu_temp_stat = {
                            let stat = Stat::temp(snap.cpu_temp);
                            match current_enable_phase(
                                &self.enable_phase,
                                snap.sensor_service_alive,
                            ) {
                                EnablePhase::Idle if snap.sensor_service_alive => stat,
                                EnablePhase::Idle => stat
                                    .with_action(StatAction::Restart)
                                    .with_failure(snap.sensor_failure.clone()),
                                _ => stat.with_action(StatAction::Busy),
                            }
                        };
                        let restart_clicked = metric_card(
                            ui,
                            "CPU",
                            cpu_sub,
                            egui::Color32::from_rgb(90, 170, 255),
                            &[
                                cpu_temp_stat,
                                Stat::new("LOAD", snap.cpu_load.map(|v| format!("{v:.0}%"))),
                                Stat::new(
                                    "FREQ",
                                    snap.cpu_clock.map(|v| format!("{:.2} GHz", v / 1000.0)),
                                ),
                                Stat::new("PWR", snap.cpu_power.map(|v| format!("{v:.0} W"))),
                            ],
                            Some((&snap.cpu_cores, &snap.cpu_core_clocks)),
                            &snap.cpu_hist,
                            None,
                        );

                        if restart_clicked {
                            enable_sensors(&self.enable_phase);
                        }

                        ui.add_space(12.0);

                        // One card per GPU.
                        let empty = Series::default();
                        for (i, gpu) in snap.gpus.iter().enumerate() {
                            let title = if snap.gpus.len() > 1 {
                                format!("GPU {}", i + 1)
                            } else {
                                "GPU".to_string()
                            };
                            metric_card(
                                ui,
                                &title,
                                Some(gpu.name.as_str()),
                                GPU_ACCENTS[i % GPU_ACCENTS.len()],
                                &[
                                    Stat::temp(gpu.temp),
                                    Stat::new("LOAD", gpu.load.map(|v| format!("{v:.0}%")))
                                        .with_hint(gpu.load_source),
                                    Stat::new("CLK", gpu.clock.map(|v| format!("{v:.0} MHz"))),
                                    Stat::new("PWR", gpu.power.map(|v| format!("{v:.0} W"))),
                                ],
                                None,
                                snap.gpu_hists.get(i).unwrap_or(&empty),
                                match (gpu.mem_used_mb, gpu.mem_total_mb) {
                                    (Some(u), Some(t)) => Some((u, t)),
                                    _ => None,
                                },
                            );
                            ui.add_space(12.0);
                        }

                        // MEM card
                        memory_card(
                            ui,
                            snap.mem_used_pct,
                            snap.mem_used_gb,
                            snap.mem_total_gb,
                            &snap.mem_hist,
                        );

                        ui.add_space(12.0);

                        egui::CollapsingHeader::new(format!(
                            "All sensors ({})",
                            snap.sensors.len()
                        ))
                        .default_open(false)
                        .show(ui, |ui| {
                            for (label, value) in &snap.sensors {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new(label).size(11.0));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(format!("{value:.1} °C"))
                                                    .size(11.0)
                                                    .color(temp_color(*value)),
                                            );
                                        },
                                    );
                                });
                            }
                        });
                    });
            });

        // Separate "Device Info" window (CPU-Z / GPU-Z style), on demand.
        if self.show_info {
            let mut open = true;
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("device_info"),
                egui::ViewportBuilder::default()
                    .with_title("Device Info")
                    .with_inner_size([480.0, 640.0])
                    .with_position([430.0, 80.0]),
                |ctx, _class| {
                    const TABS: [&str; 5] = ["CPU", "GPU", "Memory", "Mainboard", "Tests"];
                    egui::TopBottomPanel::top("info_tabs").show(ctx, |ui| {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            for (i, name) in TABS.iter().enumerate() {
                                if ui.selectable_label(self.info_tab == i, *name).clicked() {
                                    self.info_tab = i;
                                }
                            }
                        });
                        ui.add_space(4.0);
                    });
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            let s = self.shared.lock().unwrap();
                            match s.device_info.as_ref() {
                                None => {
                                    ui.add_space(20.0);
                                    ui.label("Gathering device information…");
                                }
                                Some(info) => match self.info_tab {
                                    1 => {
                                        for (i, (name, rows)) in info.gpus.iter().enumerate() {
                                            if i > 0 {
                                                ui.add_space(14.0);
                                            }
                                            info_section(ui, name, rows);
                                        }
                                    }
                                    2 => {
                                        info_section(ui, "Memory", &info.mem_summary);
                                        ui.add_space(10.0);
                                        dimm_grid(ui, &info.mem_modules);
                                    }
                                    3 => info_section(ui, "Mainboard", &info.mainboard),
                                    4 => bench_tab(ui, &self.bench),
                                    _ => {
                                        info_section(ui, "CPU", &info.cpu);
                                        if !info.cpu_isa.is_empty() {
                                            ui.add_space(14.0);
                                            info_section(ui, "Instructions", &info.cpu_isa);
                                        }
                                    }
                                },
                            }
                        });
                    });
                    if ctx.input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                },
            );
            if !open {
                self.show_info = false;
            }
        }

        // A stress test's readings and Stop button live in the Device Info window, so
        // it shouldn't keep heating the machine once that window has gone.
        if !self.show_info {
            self.bench.stop_all_stress();
        }

        // Nobody's looking while the window is in the tray, so redraw rarely.
        let redraw_ms = if tray::main_hidden() { 1000 } else { 200 };
        ctx.request_repaint_after(Duration::from_millis(redraw_ms));
    }
}

/// What a press on a test card asks for.
#[derive(PartialEq)]
enum BenchAction {
    None,
    Run,
    Stress,
    StopStress,
}

/// The Tests tab: one card per device, each with its own buttons.
fn bench_tab(ui: &mut egui::Ui, bench: &BenchRunner) {
    let slots = bench.snapshot();
    let speed_test_running = slots
        .iter()
        .any(|s| matches!(s.status, BenchStatus::Running(_)));
    let stress_running = slots.iter().any(|s| s.status == BenchStatus::Stressing);
    ui.label(
        egui::RichText::new(
            "Run measures speed in a few seconds. Start stress holds full load until you stop it, watching for overheating and errors.",
        )
        .size(11.0)
        .color(egui::Color32::GRAY),
    );
    ui.add_space(8.0);
    for (i, slot) in slots.iter().enumerate() {
        let can_run = !speed_test_running && !stress_running;
        match bench_card(ui, slot, can_run, !speed_test_running) {
            BenchAction::Run => bench.start(i),
            BenchAction::Stress => bench.start_stress(i),
            BenchAction::StopStress => bench.stop_stress(i),
            BenchAction::None => {}
        }
        ui.add_space(8.0);
    }
}

fn bench_card(ui: &mut egui::Ui, slot: &BenchSlot, can_run: bool, can_stress: bool) -> BenchAction {
    let accent = match slot.kind {
        BenchKind::Cpu => egui::Color32::from_rgb(90, 170, 255),
        BenchKind::Memory => egui::Color32::from_rgb(180, 140, 255),
        BenchKind::Gpu => GPU_ACCENTS[0],
    };
    let mut action = BenchAction::None;
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(30, 33, 39))
        .rounding(8.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                ui.painter().rect_filled(r, 1.5, accent);
                ui.label(egui::RichText::new(&slot.name).size(13.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(can_run, egui::Button::new("Run")).clicked() {
                        action = BenchAction::Run;
                    }
                    if slot.kind != BenchKind::Memory {
                        if slot.status == BenchStatus::Stressing {
                            if ui.button("Stop stress").clicked() {
                                action = BenchAction::StopStress;
                            }
                        } else if ui.add_enabled(can_stress, egui::Button::new("Start stress")).clicked() {
                            action = BenchAction::Stress;
                        }
                    }
                });
            });
            ui.add_space(6.0);

            let mut group = "";
            for row in &slot.rows {
                if row.group != group {
                    group = row.group;
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(group)
                            .size(10.0)
                            .color(egui::Color32::GRAY),
                    );
                }
                info_row(ui, row.label, &row.value, 160.0);
            }

            match &slot.status {
                BenchStatus::Idle => {
                    ui.label(
                        egui::RichText::new("Not run yet")
                            .size(11.0)
                            .color(egui::Color32::GRAY),
                    );
                }
                BenchStatus::Running(step) => {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(13.0));
                        ui.label(egui::RichText::new(format!("{step}…")).size(11.0));
                    });
                }
                BenchStatus::Stressing => {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(13.0));
                        ui.label(egui::RichText::new("Stress test running…").size(11.0));
                    });
                }
                BenchStatus::Done => {}
                BenchStatus::Failed(message) => {
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(message)
                                .size(11.0)
                                .color(egui::Color32::from_rgb(240, 90, 90)),
                        )
                        .wrap(),
                    );
                }
            }
        });
    action
}

/// Render DIMM modules as per-slot cards, two across.
fn dimm_grid(ui: &mut egui::Ui, modules: &[(String, Vec<(String, String)>)]) {
    let accent = egui::Color32::from_rgb(180, 140, 255);
    for (row_idx, chunk) in modules.chunks(2).enumerate() {
        let _ = row_idx;
        ui.columns(2, |cols| {
            for (col_idx, (title, rows)) in chunk.iter().enumerate() {
                dimm_card(&mut cols[col_idx], title, rows, accent);
            }
        });
        ui.add_space(8.0);
    }
}

fn dimm_card(ui: &mut egui::Ui, title: &str, rows: &[(String, String)], accent: egui::Color32) {
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(30, 33, 39))
        .rounding(8.0)
        .inner_margin(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(3.0, 14.0), egui::Sense::hover());
                ui.painter().rect_filled(r, 1.5, accent);
                ui.label(egui::RichText::new(title).size(13.0).strong());
            });
            ui.add_space(6.0);
            for (k, v) in rows {
                info_row(ui, k, v, 54.0);
            }
        });
}

fn info_section(ui: &mut egui::Ui, title: &str, rows: &[(String, String)]) {
    ui.label(egui::RichText::new(title).size(14.0).strong());
    ui.separator();
    for (k, v) in rows {
        info_row(ui, k, v, 132.0);
    }
}

/// One key/value row: fixed-width label, value wraps to the remaining width.
fn info_row(ui: &mut egui::Ui, key: &str, value: &str, key_w: f32) {
    ui.horizontal_top(|ui| {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(key_w, ui.text_style_height(&egui::TextStyle::Body)), egui::Sense::hover());
        ui.painter().text(
            rect.left_top(),
            egui::Align2::LEFT_TOP,
            key,
            egui::FontId::proportional(13.0),
            egui::Color32::GRAY,
        );
        ui.add(egui::Label::new(egui::RichText::new(value).size(13.0)).wrap());
    });
    ui.add_space(3.0);
}

struct Snapshot {
    status: String,
    cpu_name: String,
    cpu_temp: Option<f32>,
    cpu_clock: Option<f32>,
    cpu_load: Option<f32>,
    cpu_power: Option<f32>,
    cpu_cores: Vec<f32>,
    cpu_core_clocks: Vec<f32>,
    sensor_service_alive: bool,
    sensor_failure: Option<String>,
    gpus: Vec<GpuState>,
    mem_used_pct: Option<f32>,
    mem_used_gb: f32,
    mem_total_gb: f32,
    sensors: Vec<(String, f32)>,
    cpu_hist: Series,
    mem_hist: Series,
    gpu_hists: Vec<Series>,
}

/// What a stat shows in place of a missing value.
#[derive(Clone, Copy, PartialEq)]
enum StatAction {
    /// A dash: the value is simply unavailable.
    None,
    /// A clickable restart icon.
    Restart,
    /// A spinner: a restart is in progress.
    Busy,
}

struct Stat {
    label: String,
    value: Option<String>,
    is_temp: bool,
    temp_value: f32,
    action: StatAction,
    hint: &'static str,
    /// Why the last attempt at what `Restart` offers failed.
    failure: Option<String>,
}

impl Stat {
    fn new(label: &str, value: Option<String>) -> Self {
        Self {
            label: label.to_string(),
            value,
            is_temp: false,
            temp_value: 0.0,
            action: StatAction::None,
            hint: "",
            failure: None,
        }
    }
    fn temp(t: Option<f32>) -> Self {
        Self {
            label: "TEMP".to_string(),
            value: t.map(|v| format!("{v:.0} °C")),
            is_temp: true,
            temp_value: t.unwrap_or(0.0),
            action: StatAction::None,
            hint: "",
            failure: None,
        }
    }
    fn with_action(mut self, action: StatAction) -> Self {
        self.action = action;
        self
    }
    fn with_failure(mut self, failure: Option<String>) -> Self {
        self.failure = failure;
        self
    }
    /// Tooltip explaining what the value means.
    fn with_hint(mut self, hint: &'static str) -> Self {
        self.hint = hint;
        self
    }
}

/// Clickable circular-arrow icon. Returns true when pressed.
fn restart_icon(ui: &mut egui::Ui, failure: Option<&str>) -> bool {
    let size = egui::vec2(24.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let hint = "CPU temperature, frequency and power are off.\n\
        Click to turn them on. This asks for admin rights once, to set up a small Windows \
        service and the PawnIO driver. Both can be removed from Windows' installed apps.";
    let resp = resp
        .on_hover_text(match failure {
            Some(reason) => format!("{reason}\n\n{hint}"),
            None => hint.to_string(),
        })
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    let color = if resp.hovered() {
        egui::Color32::from_rgb(255, 200, 120)
    } else {
        egui::Color32::from_rgb(220, 160, 90)
    };
    let painter = ui.painter_at(rect);
    if resp.hovered() {
        painter.rect_filled(rect, 5.0, egui::Color32::from_rgb(52, 56, 64));
    }

    // Clockwise arc with a gap at the top right; screen y points down, so
    // increasing angle runs clockwise.
    let c = rect.center();
    let r = 7.0;
    let start = -std::f32::consts::FRAC_PI_2 + 0.75;
    let sweep = 5.0;
    let at = |a: f32| c + r * egui::vec2(a.cos(), a.sin());
    let points: Vec<egui::Pos2> = (0..=28)
        .map(|i| at(start + sweep * i as f32 / 28.0))
        .collect();
    painter.add(egui::Shape::line(points, egui::Stroke::new(2.0_f32, color)));

    // Arrowhead at the end of the arc, pointing along its tangent.
    let end = start + sweep;
    let tip_dir = egui::vec2(-end.sin(), end.cos());
    let side = egui::vec2(end.cos(), end.sin());
    let base = at(end);
    painter.add(egui::Shape::convex_polygon(
        vec![
            base + tip_dir * 4.5,
            base + side * 3.5,
            base - side * 3.5,
        ],
        color,
        egui::Stroke::NONE,
    ));

    resp.clicked()
}

fn metric_card(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: Option<&str>,
    accent: egui::Color32,
    stats: &[Stat],
    cores: Option<(&[f32], &[f32])>,
    series: &Series,
    vram: Option<(f32, f32)>,
) -> bool {
    let mut restart_clicked = false;
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(30, 33, 39))
        .rounding(10.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(4.0, 16.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, accent);
                ui.label(egui::RichText::new(title).size(15.0).strong());
                if let Some(sub) = subtitle {
                    ui.label(
                        egui::RichText::new(sub)
                            .size(11.0)
                            .color(egui::Color32::GRAY),
                    );
                }
            });
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                for (i, stat) in stats.iter().enumerate() {
                    if i > 0 {
                        ui.add_space(22.0);
                    }
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(&stat.label)
                                .size(10.0)
                                .color(egui::Color32::GRAY),
                        );
                        let color = if stat.is_temp && stat.value.is_some() {
                            temp_color(stat.temp_value)
                        } else {
                            egui::Color32::from_rgb(230, 232, 236)
                        };
                        match (&stat.value, stat.action) {
                            (None, StatAction::Restart) => {
                                restart_clicked |= restart_icon(ui, stat.failure.as_deref());
                            }
                            (None, StatAction::Busy) => {
                                ui.add(egui::Spinner::new().size(20.0)).on_hover_text(
                                    "Starting the sensors. Approve the admin prompt if asked.",
                                );
                            }
                            (value, _) => {
                                let label = ui.label(
                                    egui::RichText::new(value.as_deref().unwrap_or("—"))
                                        .size(17.0)
                                        .strong()
                                        .color(color),
                                );
                                if !stat.hint.is_empty() {
                                    label.on_hover_text(stat.hint);
                                }
                            }
                        }
                    });
                }
            });

            ui.add_space(8.0);
            if let Some((loads, clocks)) = cores {
                if !loads.is_empty() {
                    core_bars(ui, loads, clocks);
                    ui.add_space(8.0);
                }
            }
            sparkline(ui, series, 100.0, accent);

            if let Some((used, total)) = vram {
                if total > 0.0 {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(format!("VRAM  {used:.0} / {total:.0} MB"))
                            .size(10.0)
                            .color(egui::Color32::GRAY),
                    );
                    ui.add_space(4.0);
                    usage_bar(ui, used / total * 100.0, accent);
                }
            }
        });
    restart_clicked
}

/// A row of per-core load bars (one bar per logical processor). Hovering a bar
/// shows that core's load and effective frequency.
fn core_bars(ui: &mut egui::Ui, loads: &[f32], clocks: &[f32]) {
    let height = 34.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    let n = loads.len();
    if n == 0 {
        return;
    }
    let gap = 3.0;
    let bw = ((rect.width() - gap * (n as f32 - 1.0)) / n as f32).max(1.0);
    for (i, &c) in loads.iter().enumerate() {
        let x = rect.left() + i as f32 * (bw + gap);
        let cell = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(bw, rect.height()));
        painter.rect_filled(cell, 2.0, egui::Color32::from_rgb(34, 37, 44));
        let t = (c / 100.0).clamp(0.0, 1.0);
        let bh = t * rect.height();
        let bar = egui::Rect::from_min_size(
            egui::pos2(x, rect.bottom() - bh),
            egui::vec2(bw, bh),
        );
        painter.rect_filled(bar, 2.0, load_color(c));

        // Per-core hover tooltip with load + frequency.
        let resp = ui.interact(cell, ui.id().with(("core", i)), egui::Sense::hover());
        if resp.hovered() {
            let freq = clocks.get(i).copied().unwrap_or(0.0);
            let freq_txt = if freq > 0.0 {
                format!("{:.2} GHz", freq / 1000.0)
            } else {
                "idle".to_string()
            };
            resp.on_hover_text(format!("Core {i} · {c:.0}% · {freq_txt}"));
        }
    }
}

/// Load-based color: calm blue when idle, warming toward red when saturated.
fn load_color(load: f32) -> egui::Color32 {
    if load < 50.0 {
        egui::Color32::from_rgb(90, 170, 255)
    } else if load < 85.0 {
        egui::Color32::from_rgb(240, 180, 80)
    } else {
        egui::Color32::from_rgb(240, 90, 90)
    }
}

fn memory_card(
    ui: &mut egui::Ui,
    used_pct: Option<f32>,
    used_gb: f32,
    total_gb: f32,
    series: &Series,
) {
    let accent = egui::Color32::from_rgb(180, 140, 255);
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(30, 33, 39))
        .rounding(10.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(4.0, 16.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, accent);
                ui.label(egui::RichText::new("MEM").size(15.0).strong());
                if total_gb > 0.0 {
                    ui.label(
                        egui::RichText::new(format!("{used_gb:.1} / {total_gb:.1} GB"))
                            .size(11.0)
                            .color(egui::Color32::GRAY),
                    );
                }
            });
            ui.add_space(8.0);

            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("USED")
                        .size(10.0)
                        .color(egui::Color32::GRAY),
                );
                ui.label(
                    egui::RichText::new(
                        used_pct
                            .map(|v| format!("{v:.0}%"))
                            .unwrap_or_else(|| "—".to_string()),
                    )
                    .size(17.0)
                    .strong()
                    .color(egui::Color32::from_rgb(230, 232, 236)),
                );
            });

            ui.add_space(8.0);
            usage_bar(ui, used_pct.unwrap_or(0.0), accent);
            ui.add_space(8.0);
            sparkline(ui, series, 100.0, accent);
        });
}

/// A horizontal track with a proportional fill (0..100).
fn usage_bar(ui: &mut egui::Ui, pct: f32, color: egui::Color32) {
    let height = 10.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 5.0, egui::Color32::from_rgb(34, 37, 44));
    let w = (pct / 100.0).clamp(0.0, 1.0) * rect.width();
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height()));
    painter.rect_filled(fill, 5.0, color);
}

fn sparkline(ui: &mut egui::Ui, series: &Series, max: f32, color: egui::Color32) {
    let height = 42.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, egui::Color32::from_rgb(24, 26, 31));

    let n = series.data.len();
    let dx = rect.width() / (HISTORY.max(2) - 1) as f32;
    // Newest point on the right edge, older points stepping left.
    let x0 = rect.right() - dx * n.saturating_sub(1) as f32;
    let point = |i: usize, v: f32| -> egui::Pos2 {
        let t = (v / max).clamp(0.0, 1.0);
        egui::pos2(x0 + dx * i as f32, rect.bottom() - t * rect.height())
    };

    // Fill: one vertical strip per pair of neighbouring points. Each strip is
    // convex even when the graph as a whole isn't, so jagged data draws cleanly.
    let fill = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40);
    let runs = series.runs();
    let mut mesh = egui::epaint::Mesh::default();
    for run in &runs {
        for pair in run.windows(2) {
            let (a, b) = (point(pair[0].0, pair[0].1), point(pair[1].0, pair[1].1));
            let first = mesh.vertices.len() as u32;
            mesh.colored_vertex(a, fill);
            mesh.colored_vertex(b, fill);
            mesh.colored_vertex(egui::pos2(a.x, rect.bottom()), fill);
            mesh.colored_vertex(egui::pos2(b.x, rect.bottom()), fill);
            mesh.add_triangle(first, first + 1, first + 2);
            mesh.add_triangle(first + 1, first + 2, first + 3);
        }
    }
    painter.add(egui::Shape::mesh(mesh));

    // Line: drawn per run, so a gap in the data is a gap in the line.
    for run in runs {
        let line: Vec<egui::Pos2> = run.iter().map(|&(i, v)| point(i, v)).collect();
        if line.len() == 1 {
            painter.circle_filled(line[0], 1.0, color);
        } else {
            painter.add(egui::Shape::line(line, egui::Stroke::new(1.5_f32, color)));
        }
    }

    // Emphasize the current value, if there is one.
    if let Some(Some(v)) = series.data.back() {
        painter.circle_filled(point(n - 1, *v), 2.5, color);
    }
}

fn temp_color(t: f32) -> egui::Color32 {
    if t < 60.0 {
        egui::Color32::from_rgb(120, 220, 140)
    } else if t < 80.0 {
        egui::Color32::from_rgb(240, 180, 80)
    } else {
        egui::Color32::from_rgb(240, 90, 90)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_splits_into_runs_at_gaps() {
        let mut s = Series::default();
        for v in [Some(1.0), Some(2.0), None, None, Some(3.0), None, Some(4.0), Some(5.0)] {
            s.push(v);
        }
        assert_eq!(
            s.runs(),
            vec![
                vec![(0, 1.0), (1, 2.0)],
                vec![(4, 3.0)],
                vec![(6, 4.0), (7, 5.0)],
            ]
        );
    }

    #[test]
    fn series_keeps_only_the_latest_points() {
        let mut s = Series::default();
        for i in 0..HISTORY + 5 {
            s.push(Some(i as f32));
        }
        assert_eq!(s.data.len(), HISTORY);
        assert_eq!(s.data.front(), Some(&Some(5.0)));
    }
}

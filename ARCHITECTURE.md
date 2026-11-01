# Architecture

A small desktop system monitor for Windows showing live **CPU / GPU temperature,
frequency and load**.

The app (`eframe`/`egui`) is pure Rust and **runs without admin rights**. Most hardware
is read directly from it: GPUs through NVIDIA's **NVML** and AMD's **ADL**, and CPU load
and memory through Windows. There is no .NET.

CPU temperature, frequency and power need the **PawnIO** kernel driver, and the driver
needs admin rights, so a small Windows service running as SYSTEM reads them and sends the
values to the app over a read-only named pipe. The app never touches the driver.

> Earlier versions used a C# `sensorbridge` child process wrapping
> `LibreHardwareMonitorLib`, then the WinRing0 driver through an elevated helper process.
> Both were replaced; see the git history.

```
┌─────────────────────────────────────────────────────────────┐
│  monitor.exe  (Rust / egui)                  — not elevated │
│                                                             │
│  UI thread ◀── Arc<Mutex<Shared>> ◀── sampler thread        │
│                                        │        │           │
│                               pipe client   NVML, ADL       │
└────────────────────────────────────────┼────────┼───────────┘
                  read-only named pipe  │        │
                                        │        ▼
┌────────────────────────────────────────┼─┐   GPU drivers
│  monitor.exe --service     — SYSTEM    │ │   (no admin)
│                                        │ │
│  pipe server ◀── sampler ──────────────┘ │
│                     │                    │
└─────────────────────┼────────────────────┘
                      ▼
              PawnIO kernel driver
                      │
                      ▼
            CPU registers (MSR / SMN)
```

## Module layout

| File | Role |
|---|---|
| `src/main.rs` | entry point, egui UI, `Shared` state struct |
| `src/tray.rs` | tray icon and taskbar widget |
| `src/logging.rs` | log files, shared with the `setup` crate |
| `src/update.rs` | daily check for a newer release |
| `src/sensors/mod.rs` | `Sampler`: the app's background thread, writes `Shared` |
| `src/sensors/service.rs` | the service, its named pipe, and the app's client for it |
| `src/sensors/cpu.rs` | CPU temperature, clocks and power, for AMD and Intel |
| `src/sensors/pawnio.rs` | module loading, MSR / SMN reads |
| `src/sensors/pawnio_device.rs` | opening the driver, shared with the `setup` crate |
| `src/sensors/setup_client.rs` | starts `monitor-setup.exe` and reports its result |
| `src/sensors/sysload.rs` | CPU load and memory |
| `src/sensors/gpu.rs` | GPU vendor seam, with `nvml.rs` and `adl.rs` behind it |
| `src/sensors/deviceinfo.rs` | device specs for the Info window |
| `src/sensors/bench.rs` | benchmarks and stress tests |
| `src/setup_exit.rs` | exit codes of `monitor-setup.exe`, shared with the `setup` crate |
| `setup/` | `monitor-setup.exe`: installs and removes the service and driver |
| `installer/` | MSI definition, `.bat` files, licences, `build-release.ps1` |

Data flow: the app's **sampler thread** samples every 500 ms and adds a history point once
a second, writing into `Arc<Mutex<Shared>>`; the **UI thread** snapshots that each frame.
CPU temperature, frequency and power arrive from the service; if it isn't running, the TEMP
stat offers to install it.

## Install

| | MSI | Portable zip |
|---|---|---|
| **Admin prompt** | Once, at install | Once, when the sensors are first enabled |
| **Installed by** | The MSI itself | `monitor-setup.exe`, from the sensor icon or `install-sensors.bat` |
| **App folder** | `%ProgramFiles%\Monitor` | Wherever it was unzipped |
| **Service runs** | `monitor.exe --service`, from the app folder | `monitor-sensors.exe --service`, a copy in `%ProgramFiles%\Monitor Sensors` |

`monitor.exe` has no install code and asks for no admin rights. `monitor-setup.exe` has a
manifest that requires them. The service only ever runs from a folder under Program Files:
one the user can write to would hand out SYSTEM rights. Installing the MSI replaces a
portable copy's service, and `monitor-setup.exe uninstall` leaves the MSI's service alone.

## CPU — `cpu.rs` + `pawnio.rs`

Reading a CPU die temperature needs kernel-mode access to CPU registers, so we use a signed
kernel driver (**PawnIO**) and talk to it via `DeviceIoControl`. It runs signed modules that
allow only the registers they list.

- **Driver lifecycle**: the MSI or `monitor-setup.exe` runs the official setup
  (`-install -silent`) if the driver is missing. The service opens
  `\\?\GLOBALROOT\Device\PawnIO` and loads `AMDFamily17` or `IntelMSR`, only while a client
  is connected. The driver stays installed when Monitor is removed, as other apps share it.
- **Pipe**: `\\.\pipe\MonitorSensors`. Signed-in users may only read. The client checks the
  other end is in session 0, in case a program took the name while the service was down.
- **Temperature (AMD)**: SMN thermal register `0x00059800`, via the module's
  `ioctl_read_smn`, under the shared `Global\Access_PCI` lock;
  `temp = ((raw>>21)&0x7FF)*0.125`, minus 49 when the range-select bit (`0x80000`) is set
  (as it is on this Phoenix/Hawk-Point APU).
- **Temperature (Intel)**: Tjmax minus the `IA32_PACKAGE_THERM_STATUS` readout.
- **Clock**: per-core `APERF`/`MPERF` MSRs sampled over ~120 ms (thread pinned to each
  core), ratio × base frequency → effective clock, max core. AMD's module only allows their
  read-only addresses (`0xC00000E7`/`E8`).
- **Power**: RAPL package energy counter, as the change since the previous reading.
- **Load**: `GetSystemTimes` since the previous reading, in the app. Vendor-neutral.
- **Name**: CPUID brand string (leaves `0x80000002..0x80000004`).

## GPU — `gpu.rs`

Every GPU that a backend can see gets a card. **No admin needed.**

- **NVIDIA**: `nvml.rs`, through the driver's NVML library.
- **AMD**: `adl.rs`, through `atiadlxx.dll` (**ADL**), dynamically loaded.
  `ADL2_Main_Control_Create` → `ADL2_Adapter_AdapterInfo_Get` → pick the AMD adapter
  (vendor id **1002 decimal**), then `ADL2_New_QueryPMLogData_Get`. From the PMLog sensor
  array: **clock** = id 1 (GfxClock, MHz), **load** = id 19 (Gfx activity, %),
  **temperature** = first supported of ids 32/29/28/20/8. On the integrated 780M the
  discrete "edge" sensor (id 8) is unsupported; the SoC/VR sensors carry the temperature.

## The driver files

PawnIO's files are **not** committed. `fetch-pawnio.ps1` downloads them into `driver/`, at
pinned versions and hashes:

- `AMDFamily17.bin` and `IntelMSR.bin`, signed modules that are **embedded into the exe** at
  build time via `include_bytes!`.
- `PawnIO_setup.exe`, the official setup, which **ships beside the exe** in the MSI and zip.

PawnIO is © namazso: the driver is GPL-2.0-or-later with an exception for programs using
its device interface, and the modules are LGPL-2.1-or-later. See `installer/licenses/`.

`build-release.ps1` also generates `RUST-LICENSES.txt` for the packages with `cargo about`,
installing it if needed. `installer/about.toml` lists the licences it accepts.

## Vendor scope

| Metric | AMD | Intel | NVIDIA |
|---|---|---|---|
| CPU load | `GetSystemTimes` (neutral) | same | n/a |
| CPU clock | `APERF`/`MPERF` | same | n/a |
| CPU temp | SMN registers | `IA32_PACKAGE_THERM_STATUS` + Tjmax | n/a |
| CPU power | RAPL | RAPL | n/a |
| GPU | ADL (`atiadlxx.dll`) | not supported yet | NVML (`nvml-wrapper` crate) |

The development hardware is a Ryzen 7 8745H with a Radeon 780M and an RTX 5060 Ti. The
Intel CPU paths have not been run on Intel hardware.

## Building

```powershell
.\fetch-pawnio.ps1               # obtain the files in driver\ (once)
cargo build --release            # -> target\release\monitor.exe and monitor-setup.exe
.\installer\build-release.ps1    # -> the MSI and portable zip in target\installer
```

A development build runs from `target\debug` and works like a portable copy, except that it
keeps its data in `%APPDATA%\Monitor`. Enabling its sensors installs that debug build as the
service, so installing the MSI is usually the better way to get them while developing.

## Releasing

CI (`.github/workflows/ci.yml`) runs Clippy and the tests and builds the packages on every
push and pull request.

To release, set the version in `[workspace.package]` in `Cargo.toml`, commit, and push a
tag:

```powershell
git tag v0.2.0
git push origin main v0.2.0
```

`release.yml` then runs the tests, builds everything and creates a **draft** GitHub release.
Try the draft's MSI, then publish it. Publishing is what installed copies notice: they read
`releases/latest/download/latest.txt` about 30 seconds after starting and then daily, and
offer the new MSI in the tray menu. The upgrade itself shows a UAC prompt. Portable copies
don't check.

### Signing

Without signing settings, releases are unsigned. To turn on SignPath:

1. Set up the project at SignPath, with GitHub as a trusted build system, and with
   `exes` and `msi` artifact configurations: a `zip-file` root holding the two `.exe` files,
   and a `zip-file` root holding the `.msi`.
2. In the GitHub repo's Actions settings, add the secret `SIGNPATH_API_TOKEN` and the
   variables `SIGNPATH_ORGANIZATION_ID`, `SIGNPATH_PROJECT_SLUG` and `SIGNPATH_POLICY_SLUG`.

`release.yml` then signs the exes before packaging them, signs the MSI after, and fails if a
signature isn't valid. Its pinned `PawnIO_setup.exe` is already signed by its author.

## Hardware notes

ADL exposes limited sensors for the integrated GPU — the SoC/VR temperatures rather than a
discrete edge/hotspot/VRAM set — which is expected for an APU.

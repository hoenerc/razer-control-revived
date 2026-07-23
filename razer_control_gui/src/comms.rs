use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};

/// Razer laptop control socket path.
/// Prefer XDG_RUNTIME_DIR (/run/user/<uid>) which persists for the session.
/// Fall back to /tmp for AppImage or environments without XDG_RUNTIME_DIR.
pub fn socket_path() -> String {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        format!("{}/razercontrol-socket", dir)
    } else {
        "/tmp/razercontrol-socket".to_string()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GpuInfo {
    pub name: String,
    pub pci_slot: String,
    pub driver: String,
    pub gpu_type: String,
    pub runtime_status: String,
}

/// A dGPU sensor snapshot cached by the daemon's fan-curve task. Values exist
/// only while a smart curve with a GPU/Both temperature source is actively
/// sampling (and the dGPU is runtime-active); `age_ms` is the snapshot's age
/// so clients can tell live data from leftovers. This is the only way any
/// client gets dGPU sensor values — nobody besides the daemon's curve task
/// ever runs nvidia-smi.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct DgpuSensors {
    pub temp_c: f64,
    pub power_w: Option<f64>,
    pub util_pct: Option<u32>,
    pub age_ms: u64,
}

/// A single temperature -> fan-speed point on a smart fan curve.
/// Points are kept sorted by `temp_c` ascending.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub struct FanCurvePoint {
    pub temp_c: u8,
    pub rpm: u16,
}

/// Which temperature drives the smart fan curve.
///
/// `Both` does NOT mean max(cpuTemp, gpuTemp): the CPU temp is looked up on the
/// CPU curve and the GPU temp on the GPU curve, and whichever lookup yields the
/// higher RPM wins (mirrors Synapse's activeTemperatureMode + useBothTemperatures).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum CurveTempSource {
    Cpu,
    Gpu,
    Both,
}

/// A smart fan curve: the daemon evaluates this continuously and drives the fans
/// in manual mode. Stored per AC state so AC and battery can differ.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FanCurve {
    pub enabled: bool,
    pub source: CurveTempSource,
    /// Used when `source` is `Cpu` or `Both`.
    pub cpu_points: Vec<FanCurvePoint>,
    /// Used when `source` is `Gpu` or `Both`.
    pub gpu_points: Vec<FanCurvePoint>,
}

impl FanCurve {
    #[allow(dead_code)]
    pub fn new() -> FanCurve {
        FanCurve {
            enabled: false,
            source: CurveTempSource::Cpu,
            cpu_points: default_curve_points(),
            gpu_points: default_curve_points(),
        }
    }
}

/// A gentle default curve spanning a typical laptop fan range. Points outside a
/// given model's range are clamped to that range when applied to hardware.
#[allow(dead_code)]
pub fn default_curve_points() -> Vec<FanCurvePoint> {
    vec![
        FanCurvePoint { temp_c: 40, rpm: 2200 },
        FanCurvePoint { temp_c: 50, rpm: 2600 },
        FanCurvePoint { temp_c: 60, rpm: 3200 },
        FanCurvePoint { temp_c: 70, rpm: 3900 },
        FanCurvePoint { temp_c: 80, rpm: 4500 },
        FanCurvePoint { temp_c: 90, rpm: 5000 },
    ]
}

#[derive(Serialize, Deserialize, Debug)]
/// Represents data sent TO the daemon
pub enum DaemonCommand {
    SetFanSpeed { ac: usize, rpm: i32 },      // Fan speed
    GetFanSpeed { ac: usize },                 // Get (Fan speed)
    SetPowerMode { ac: usize, pwr: u8, cpu: u8, gpu: u8}, // Power mode
    GetPwrLevel { ac: usize },                 // Get (Power mode)
    GetCPUBoost { ac: usize },                 // Get (CPU boost)
    GetGPUBoost { ac: usize },                 // Get (GPU boost)
    SetLogoLedState{ ac:usize, logo_state: u8 },
    GetLogoLedState { ac: usize },
    SetBrightness { ac:usize, val: u8 },
    GetBrightness { ac: usize },
    SetBatteryHealthOptimizer { is_on: bool, threshold: u8 },
    GetBatteryHealthOptimizer (),
    GetDeviceName,
    GetActualFanRpm,
    // v2.8 scope cut: SetDgpuRuntimePM and SetGpuMode (envycontrol) were
    // REMOVED here. v2.10 lighting cut: the whole effect/sync/idle surface
    // (SetEffect, SetStandardEffect, GetStandardEffect, GetKeyboardRGB,
    // SetSync, GetSync, SetIdle) was REMOVED under the same coordinated-break
    // rule — daemon and all clients ship together via install.sh. Removing mid-enum variants shifts bincode's variant
    // indices — legal only as a coordinated break with daemon + all clients
    // rebuilt and redeployed together, which install.sh guarantees. Routine
    // protocol evolution stays append-only (see the note at the enum's end).
    GetGpuStatus,
    SetFanCurve { ac: usize, curve: FanCurve },
    GetFanCurve { ac: usize },
    // Appended last on purpose: bincode identifies enum variants by index, so
    // new commands must only ever be added at the END to keep a mixed pair of
    // old/new daemon and clients from misreading each other mid-upgrade.
    GetDgpuSensors,
    SetExperimentalProfiles { enabled: bool },
    GetExperimentalProfiles,
    // v2.10 static-only lighting model: exactly one keyboard colour.
    SetStaticColor { red: u8, green: u8, blue: u8 },
    GetStaticColor,
    SetStaticLighting { enabled: bool },
    GetStaticLighting,
    // v2.13 per-model matrix: clients stop hardcoding profile lists; the
    // daemon answers with the EFFECTIVE surface (model + experimental
    // already applied).
    GetCapabilities { ac: usize },
    // charger domain: raw EC adapter class (0x07/0x8c, args[0]). None on EC
    // read failure. Appended at the end per the bincode variant-order rule.
    GetCharger,
    // Power-key cycle: the daemon reads the live charger domain + active EC
    // wire and either heals (domain changed / warm-boot re-init / hot-swap) or
    // advances one step. Replaces the client-side multi-roundtrip cycle so the
    // whole decision — and any healing — lives behind one chokepoint.
    CyclePowerMode,
    GetDesiredState,
    GetEcPowerZone { zone: u8 },
    GetEcBoost { gpu: bool },
    GetEcBrightness,
    GetEcBho,
    GetEcFanTach { zone: u8 },
    GetEcFanSetpoint { zone: u8 },
}

/// Desired-state snapshot on the wire: the single evaluation, resolved
/// against the live domain. Primitive fields only; fan_mode 0=Auto 1=Manual
/// 2=Curve (fan_rpm meaningful for Manual only).
#[derive(Serialize, Deserialize, Debug)]
pub struct DesiredStateWire {
    pub domain: ChargerDomainWire,
    pub wire: u8,
    pub boosts: Option<(u8, u8)>,
    pub brightness: u8,
    pub logo: u8,
    pub fan_mode: u8,
    pub fan_rpm: i32,
    pub bho_on: bool,
    pub bho_threshold: u8,
    /// static_lighting master switch: false = the daemon performs zero
    /// keyboard-lighting writes (external-RGB handover).
    pub lighting: bool,
}


#[derive(Serialize, Deserialize, Debug)]
/// Represents data sent back from Daemon after it receives
/// a command.
pub enum DaemonResponse {
    SetFanSpeed { result: bool },                    // Response
    GetFanSpeed { rpm: i32 },                        // Get (Fan speed)
    SetPowerMode { result: bool },                   // Response
    GetPwrLevel { pwr: u8 },                         // Get (Power mode)
    GetCPUBoost { cpu: u8 },                         // Get (CPU boost)
    GetGPUBoost { gpu: u8 },                         // Get (GPU boost)
    SetLogoLedState {result: bool },
    GetLogoLedState { logo_state: u8 },
    SetBrightness { result: bool },
    GetBrightness { result: u8 },
    SetBatteryHealthOptimizer { result: bool },
    GetBatteryHealthOptimizer { is_on: bool, threshold: u8 },
    GetDeviceName { name: String },
    GetActualFanRpm { rpm: i32 },
    GetGpuStatus {
        gpus: Vec<GpuInfo>,
        dgpu_runtime_pm: bool,
    },
    SetFanCurve { result: bool },
    GetFanCurve { curve: FanCurve },
    // Appended last — see the DaemonCommand note on bincode variant order.
    GetDgpuSensors { sensors: Option<DgpuSensors> },
    SetExperimentalProfiles { result: bool },
    GetExperimentalProfiles { enabled: bool },
    SetStaticColor { result: bool },
    GetStaticColor { color: [u8; 3] },
    SetStaticLighting { result: bool },
    GetStaticLighting { enabled: bool },
    GetCapabilities { wires: Vec<u8>, max_boost_tier: u8, model: String },
    // Raw adapter class byte from the EC; None if the EC read failed.
    GetCharger { actp: Option<u8> },
    // Result of a power-key press: the wire actually applied, the charger
    // domain (for OSD/icon), and whether this was a COLD press (re-applied
    // the current domain's profile — confirm/re-assert/heal) or a warm one
    // (advanced the cycle). None = the press could not complete.
    CyclePowerMode { applied: Option<CycleResult> },
    GetDesiredState { state: Option<DesiredStateWire> },
    GetEcPowerZone { mode: Option<(u8, u8)> },
    GetEcBoost { value: Option<u8> },
    GetEcBrightness { value: Option<u8> },
    GetEcBho { state: Option<(bool, u8)> },
    GetEcFanTach { rpm: Option<u16> },
    GetEcFanSetpoint { rpm: Option<u16> },
}

/// Outcome of a power-key press, enough for the client to render an OSD
/// without re-querying. `cold` = this press (re-)applied the current profile
/// (first press, or domain changed); a warm press within the advance window
/// cycled one step instead.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct CycleResult {
    pub wire: u8,
    /// "AC" (barrel), "PD", or "battery" — for the OSD subtitle/icon.
    pub domain: ChargerDomainWire,
    pub cold: bool,
}

/// Wire-safe mirror of the daemon's ChargerDomain (comms must not depend on the
/// daemon module). Kept in sync by hand — three variants, unlikely to change.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargerDomainWire {
    Barrel,
    Pd,
    Battery,
}

#[allow(dead_code)]
pub fn bind() -> Option<UnixStream> {
    UnixStream::connect(socket_path()).ok()
}

#[allow(dead_code)]
/// We use this from the app, but it should replace bind
pub fn try_bind() -> std::io::Result<UnixStream> {
    UnixStream::connect(socket_path())
}

#[allow(dead_code)]
pub fn create() -> Option<UnixListener> {
    let path = socket_path();
    if std::fs::metadata(&path).is_ok() {
        // Socket file exists — check if a daemon is actually listening
        if UnixStream::connect(&path).is_ok() {
            eprintln!("UNIX Socket already exists and a daemon is responding. Is another daemon running?");
            return None;
        }
        // Stale socket from a previous crash — remove it
        eprintln!("Removing stale socket file");
        if std::fs::remove_file(&path).is_err() {
            eprintln!("Could not remove stale socket file");
            return None;
        }
    }
    // Root-daemon-era relic removed: the old code forced umask 0o000 so a
    // world-writable socket let "non-root GUI/CLI" connect to a root daemon.
    // Daemon and clients have run as the SAME user for the whole life of this
    // fork, so nobody else ever needs to connect — pin the socket to 0600
    // explicitly (owner-only, deterministic regardless of inherited umask).
    // Under $XDG_RUNTIME_DIR the 0700 directory already shielded it; this
    // mainly closes the /tmp fallback, which really was world-writable.
    match UnixListener::bind(&path) {
        Ok(listener) => {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                eprintln!("Could not restrict socket permissions: {}", e);
            }
            Some(listener)
        }
        Err(e) => {
            eprintln!("Failed to bind socket: {}", e);
            None
        }
    }
}

#[allow(dead_code)]
pub fn send_to_daemon(command: DaemonCommand, mut sock: UnixStream) -> Option<DaemonResponse> {
    // Prevent blocking the GTK main thread forever if daemon is unresponsive
    let timeout = Some(std::time::Duration::from_secs(5));
    let _ = sock.set_read_timeout(timeout);
    let _ = sock.set_write_timeout(timeout);

    if let Ok(encoded) = bincode::serialize(&command) {
        if sock.write_all(&encoded).is_ok() {
            // Signal request EOF to daemon so it can read the full command.
            let _ = sock.shutdown(Shutdown::Write);

            let mut response = Vec::new();
            return match sock.read_to_end(&mut response) {
                Ok(readed) if readed > 0 => read_from_socked_resp(&response),
                Ok(_) => {
                    eprintln!("No response from daemon");
                    None
                }
                Err(error) => {
                    eprintln!("Read failed: {error}");
                    None
                }
            };
        } else {
            eprintln!("Socket write failed!");
        }
    }
    None
}

/// Deserializes incomming bytes in order to return
/// a `DaemonResponse`. None is returned if deserializing failed
fn read_from_socked_resp(bytes: &[u8]) -> Option<DaemonResponse> {
    match bincode::deserialize::<DaemonResponse>(bytes) {
        Ok(res) => {
            // debug!, not println!: the GUI polls every 2 s, and REQ/RES pairs
            // were ~55k journal lines per day. RAZER_LAPTOP_CONTROL_LOG=debug
            // re-enables them on the daemon when needed.
            log::debug!("RES: {:?}", res);
            Some(res)
        }
        Err(e) => {
            println!("RES ERROR: {}", e);
            None
        }
    }
}

/// Deserializes incomming bytes in order to return
/// a `DaemonCommand`. None is returned if deserializing failed
#[allow(dead_code)]
pub fn read_from_socket_req(bytes: &[u8]) -> Option<DaemonCommand> {
    match bincode::deserialize::<DaemonCommand>(bytes) {
        Ok(res) => {
            // See the RES note above: debug-level to keep the journal usable.
            log::debug!("REQ: {:?}", res);
            Some(res)
        }
        Err(e) => {
            println!("REQ ERROR: {}", e);
            None
        }
    }
}

/// Canonical Synapse display names per wire value — one wire, one name, on
/// every model (names sighted on a sibling 02C7 Synapse UI, 2026-07-14).
/// 0 and 6 are both "Balanced", partitioned by power domain.
pub fn profile_name(wire: u8) -> &'static str {
    match wire {
        0 | 6 => "Balanced",
        2 => "Performance",
        5 => "Silent",
        4 => "Custom",
        3 => "Battery Saver",
        7 => "Turbo",
        1 => "Gaming (legacy)",
        _ => "Unknown",
    }
}

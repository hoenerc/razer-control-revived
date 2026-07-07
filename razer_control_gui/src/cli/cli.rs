#[path = "../comms.rs"]
mod comms;
use clap::{error::ErrorKind, CommandFactory, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
// `version` without a value pulls CARGO_PKG_VERSION from Cargo.toml at build
// time — single-sourced with the GUI About page, never hardcode one here.
#[command(version, about="razer laptop configuration for linux", name="razer-cli")]
struct Cli {
    #[command(subcommand)]
    args: Args,
}

#[derive(Subcommand)]
enum Args {
    /// Read the current configuration of the device for some attribute
    Read {
        #[command(subcommand)]
        attr: ReadAttr,
    },
    /// Write a new configuration to the device for some attribute
    Write {
        #[command(subcommand)]
        attr: WriteAttr,
    },
    /// Write a standard effect
    StandardEffect {
        #[command(subcommand)]
        effect: StandardEffect,
    },
    /// Write a custom effect
    Effect {
        #[command(subcommand)]
        effect: Effect,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum OnOff {
    On,
    Off,
}

impl OnOff {
    pub fn is_on(&self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(Subcommand)]
enum ReadAttr {
    /// Read the current fan speed
    Fan(AcStateParam),
    /// Read the current power mode
    Power(AcStateParam),
    /// Read the current brightness
    Brightness(AcStateParam),
    /// Read the current logo mode
    Logo(AcStateParam),
    /// Read the current sync mode
    Sync,
    /// Read the current bho mode
    Bho,
    /// Read actual fan RPM from hardware
    FanRpm,
    /// Read GPU status information
    Gpu,
    /// Read the smart fan curve
    FanCurve(AcStateParam),
}

#[derive(Subcommand)]
enum WriteAttr {
    /// Set the fan speed
    Fan(FanParams),
    /// Set the power mode
    Power(PowerParams),
    /// Set the brightness of the keyboard
    Brightness(BrightnessParams),
    /// Set the logo mode
    Logo(LogoParams),
    /// Set sync
    Sync(SyncParams),
    /// Set battery health optimization
    Bho(BhoParams),
    /// Configure the smart fan curve
    FanCurve(FanCurveParams),
}

/// User-facing performance profiles (Blade 16 2025), partitioned by power
/// domain exactly like Synapse. `balanced` maps to a different EC wire value
/// per domain (AC=0, DC=6); the other profiles are domain-exclusive. Legacy
/// "Gaming" (wire 1) and the cooling-pad "HyperBoost" (wire 7, unsafe without
/// the pad) are deliberately not offered.
#[derive(ValueEnum, Clone, Copy)]
enum ProfileArg {
    /// AC=0 / DC=6
    Balanced,
    /// AC only (wire 2)
    Performance,
    /// AC only (wire 5)
    Silent,
    /// AC only (wire 4) — takes cpu/gpu boost 0,1,2
    Custom,
    /// battery only (wire 3)
    BatterySaver,
}

impl ProfileArg {
    /// EC wire value for the given domain (ac: 1 = plugged in, 0 = battery).
    /// `None` = this profile is not offered in that domain (Synapse parity).
    fn wire_value(self, ac: usize) -> Option<u8> {
        let plugged = ac == 1;
        match (self, plugged) {
            (ProfileArg::Balanced, true) => Some(0),
            (ProfileArg::Balanced, false) => Some(6),
            (ProfileArg::Performance, true) => Some(2),
            (ProfileArg::Silent, true) => Some(5),
            (ProfileArg::Custom, true) => Some(4),
            (ProfileArg::BatterySaver, false) => Some(3),
            _ => None,
        }
    }
    fn is_custom(self) -> bool {
        matches!(self, ProfileArg::Custom)
    }
}

#[derive(Parser)]
struct PowerParams {
    /// battery/plugged in
    ac_state: AcState,
    /// profile: balanced | performance | silent | custom | battery-saver
    profile: ProfileArg,
    /// cpu boost 0,1,2 (custom only)
    cpu_mode: Option<u8>,
    /// gpu boost 0,1,2 (custom only)
    gpu_mode: Option<u8>,
}

#[derive(Parser)]
struct FanParams {
    /// battery/plugged in
    ac_state: AcState,
    /// fan speed in RPM
    speed: i32,
}

#[derive(Parser)]
struct BrightnessParams {
    /// battery/plugged in
    ac_state: AcState,
    /// brightness
    brightness: i32,
}

#[derive(Parser)]
struct LogoParams {
    /// battery/plugged in
    ac_state: AcState,
    /// logo mode (0, 1 or 2)
    logo_state: i32,
}

#[derive(Parser)]
struct SyncParams {
    sync_state: OnOff,
}

#[derive(Parser)]
struct BhoParams {
    state: OnOff,
    /// charging threshold
    threshold: Option<u8>,
}

#[derive(Copy, Clone, ValueEnum)]
enum CurveSourceArg {
    Cpu,
    Gpu,
    Both,
}

impl CurveSourceArg {
    fn to_source(self) -> comms::CurveTempSource {
        match self {
            CurveSourceArg::Cpu => comms::CurveTempSource::Cpu,
            CurveSourceArg::Gpu => comms::CurveTempSource::Gpu,
            CurveSourceArg::Both => comms::CurveTempSource::Both,
        }
    }
}

#[derive(Parser)]
struct FanCurveParams {
    /// battery/plugged in
    ac_state: AcState,
    /// Temperature source driving the curve
    #[arg(long)]
    source: Option<CurveSourceArg>,
    /// Enable the smart fan curve
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    /// Disable the smart fan curve (back to auto/manual)
    #[arg(long)]
    disable: bool,
    /// CPU curve points as temp:rpm pairs, e.g. 40:2200,60:3000,80:4500
    #[arg(long)]
    cpu_points: Option<String>,
    /// GPU curve points as temp:rpm pairs, e.g. 40:2200,60:3000,80:4500
    #[arg(long)]
    gpu_points: Option<String>,
}

#[derive(ValueEnum, Clone)]
enum AcState {
    /// battery
    Bat,
    /// plugged in
    Ac,
}

impl AcState {
    fn as_index(&self) -> usize {
        match self {
            AcState::Bat => 0,
            AcState::Ac => 1,
        }
    }
}

#[derive(Parser, Clone)]
struct AcStateParam {
    /// battery/plugged in
    ac_state: AcState,
}

#[derive(Subcommand)]
enum StandardEffect {
    Off,
    Wave(WaveParams),
    Reactive(ReactiveParams),
    Breathing(BreathingParams),
    Spectrum,
    Static(StaticParams),
    Starlight(StarlightParams),
}

#[derive(Parser)]
struct WaveParams {
    /// direction (0 or 1)
    direction: u8,
}

#[derive(Parser)]
struct ReactiveParams {
    /// speed (0-255)
    speed: u8,
    /// red (0-255)
    red: u8,
    /// green (0-255)
    green: u8,
    /// blue (0-255)
    blue: u8,
}

#[derive(Parser)]
struct BreathingParams {
    /// kind (0-2)
    kind: u8,
    /// red1 (0-255)
    red1: u8,
    /// green1 (0-255)
    green1: u8,
    /// blue1 (0-255)
    blue1: u8,
    /// red2 (0-255)
    red2: u8,
    /// green2 (0-255)
    green2: u8,
    /// blue2 (0-255)
    blue2: u8,
}

#[derive(Parser)]
struct StarlightParams {
    /// kind (0-2)
    kind: u8,
    /// speed (0-255)
    speed: u8,
    /// red1 (0-255)
    red1: u8,
    /// green1 (0-255)
    green1: u8,
    /// blue1 (0-255)
    blue1: u8,
    /// red2 (0-255)
    red2: u8,
    /// green2 (0-255)
    green2: u8,
    /// blue2 (0-255)
    blue2: u8,
}

#[derive(Subcommand)]
enum Effect {
    Static(StaticParams),
    StaticGradient(StaticGradientParams),
    WaveGradient(WaveGradientParams),
    BreathingSingle(BreathingSingleParams),
}

#[derive(Parser)]
struct StaticParams {
    /// red (0-255)
    red: u8,
    /// green (0-255)
    green: u8,
    /// blue (0-255)
    blue: u8,
}

#[derive(Parser)]
struct StaticGradientParams {
    /// red1 (0-255)
    red1: u8,
    /// green1 (0-255)
    green1: u8,
    /// blue1 (0-255)
    blue1: u8,
    /// red2 (0-255)
    red2: u8,
    /// green2 (0-255)
    green2: u8,
    /// blue2 (0-255)
    blue2: u8,
}

#[derive(Parser)]
struct WaveGradientParams {
    /// red1 (0-255)
    red1: u8,
    /// green1 (0-255)
    green1: u8,
    /// blue1 (0-255)
    blue1: u8,
    /// red2 (0-255)
    red2: u8,
    /// green2 (0-255)
    green2: u8,
    /// blue2 (0-255)
    blue2: u8,
}

#[derive(Parser)]
struct BreathingSingleParams {
    /// red (0-255)
    red: u8,
    /// green (0-255)
    green: u8,
    /// blue (0-255)
    blue: u8,
    /// duration (0-255)
    duration: u8,
}

fn main() {
    if std::fs::metadata(comms::socket_path()).is_err() {
        eprintln!("Error. Socket doesn't exit. Is daemon running?");
        std::process::exit(1);
    }

    let cli = Cli::parse();

    match cli.args {
        Args::Read { attr } => match attr {
            ReadAttr::Fan(AcStateParam { ac_state }) => read_fan_rpm(ac_state.as_index()),
            ReadAttr::Power(AcStateParam { ac_state }) => read_power_mode(ac_state.as_index()),
            ReadAttr::Brightness(AcStateParam { ac_state }) => read_brightness(ac_state.as_index()),
            ReadAttr::Logo(AcStateParam { ac_state }) => read_logo_mode(ac_state.as_index()),
            ReadAttr::Sync => read_sync(),
            ReadAttr::Bho => read_bho(),
            ReadAttr::FanRpm => read_actual_fan_rpm(),
            ReadAttr::Gpu => read_gpu_status(),
            ReadAttr::FanCurve(AcStateParam { ac_state }) => read_fan_curve(ac_state.as_index()),
        },
        Args::Write { attr } => match attr {
            WriteAttr::Fan(FanParams { ac_state, speed }) => {
                write_fan_speed(ac_state.as_index(), speed)
            }
            WriteAttr::Power(PowerParams {
                ac_state,
                profile,
                cpu_mode,
                gpu_mode,
            }) => write_pwr_mode(ac_state.as_index(), profile, cpu_mode, gpu_mode),
            WriteAttr::Brightness(BrightnessParams {
                ac_state,
                brightness,
            }) => write_brightness(ac_state.as_index(), brightness as u8),
            WriteAttr::Sync(SyncParams { sync_state }) => write_sync(sync_state.is_on()),
            WriteAttr::Logo(LogoParams {
                ac_state,
                logo_state,
            }) => write_logo_mode(ac_state.as_index(), logo_state as u8),
            WriteAttr::Bho(BhoParams { state, threshold }) => {
                validate_and_write_bho(threshold, state)
            }
            WriteAttr::FanCurve(params) => write_fan_curve(params),
        },
        Args::Effect { effect } => match effect {
            Effect::Static(params) => send_effect(
                "static".to_string(),
                vec![params.red, params.green, params.blue],
            ),
            Effect::StaticGradient(params) => send_effect(
                "static_gradient".to_string(),
                vec![
                    params.red1,
                    params.green1,
                    params.blue1,
                    params.red2,
                    params.green2,
                    params.blue2,
                ],
            ),
            Effect::WaveGradient(params) => send_effect(
                "wave_gradient".to_string(),
                vec![
                    params.red1,
                    params.green1,
                    params.blue1,
                    params.red2,
                    params.green2,
                    params.blue2,
                ],
            ),
            Effect::BreathingSingle(params) => send_effect(
                "breathing_single".to_string(),
                vec![params.red, params.green, params.blue, params.duration],
            ),
        },
        Args::StandardEffect { effect } => match effect {
            StandardEffect::Off => send_standard_effect("off".to_string(), vec![]),
            StandardEffect::Spectrum => send_standard_effect("spectrum".to_string(), vec![]),
            StandardEffect::Breathing(params) => send_standard_effect(
                "breathing".to_string(),
                vec![
                    params.kind,
                    params.red1,
                    params.green1,
                    params.blue1,
                    params.red2,
                    params.green2,
                    params.blue2,
                ],
            ),
            StandardEffect::Reactive(params) => send_standard_effect(
                "reactive".to_string(),
                vec![params.speed, params.red, params.green, params.blue],
            ),
            StandardEffect::Starlight(params) => send_standard_effect(
                "starlight".to_string(),
                vec![
                    params.kind,
                    params.speed,
                    params.red1,
                    params.green1,
                    params.blue1,
                    params.red2,
                    params.green2,
                    params.blue2,
                ],
            ),
            StandardEffect::Static(params) => send_standard_effect(
                "static".to_string(),
                vec![params.red, params.green, params.blue],
            ),
            StandardEffect::Wave(params) => {
                send_standard_effect("wave".to_string(), vec![params.direction])
            }
        },
    }
}

fn validate_and_write_bho(threshold: Option<u8>, state: OnOff) {
    match threshold {
        Some(threshold) => {
            if !valid_bho_threshold(threshold) {
                Cli::command()
                    .error(
                        ErrorKind::InvalidValue,
                        "Threshold must be multiple of 5 between 50 and 80",
                    )
                    .exit()
            }
            write_bho(state.is_on(), threshold)
        }
        None => {
            if state.is_on() {
                Cli::command()
                    .error(
                        ErrorKind::MissingRequiredArgument,
                        "Threshold is required when BHO is on",
                    )
                    .exit()
            }
            write_bho(state.is_on(), 80)
        }
    }
}

fn read_bho() {
    send_data(comms::DaemonCommand::GetBatteryHealthOptimizer()).map_or_else(
        || eprintln!("Unknown error occured when getting bho"),
        |result| {
            if let comms::DaemonResponse::GetBatteryHealthOptimizer { is_on, threshold } = result {
                match is_on {
                    true => {
                        println!(
                            "Battery health optimization is on with a threshold of {}",
                            threshold
                        );
                    }
                    false => {
                        eprintln!("Battery health optimization is off");
                    }
                }
            }
        },
    );
}

fn write_bho(on: bool, threshold: u8) {
    if !on {
        bho_toggle_off();
        return;
    }

    bho_toggle_on(threshold);
}

fn bho_toggle_on(threshold: u8) {
    if !valid_bho_threshold(threshold) {
        eprintln!("Threshold value must be a multiple of five between 50 and 80");
        return;
    }

    send_data(comms::DaemonCommand::SetBatteryHealthOptimizer {
        is_on: true,
        threshold: threshold,
    })
    .map_or_else(
        || eprintln!("Unknown error occured when toggling bho"),
        |result| {
            if let comms::DaemonResponse::SetBatteryHealthOptimizer { result } = result {
                match result {
                    true => {
                        println!(
                            "Battery health optimization is on with a threshold of {}",
                            threshold
                        );
                    }
                    false => {
                        eprintln!("Failed to turn on bho with threshold of {}", threshold);
                    }
                }
            }
        },
    );
}

fn valid_bho_threshold(threshold: u8) -> bool {
    if threshold % 5 != 0 {
        return false;
    }

    if threshold < 50 || threshold > 80 {
        return false;
    }

    return true;
}

fn bho_toggle_off() {
    send_data(comms::DaemonCommand::SetBatteryHealthOptimizer {
        is_on: false,
        threshold: 80,
    })
    .map_or_else(
        || eprintln!("Unknown error occured when toggling bho"),
        |result| {
            if let comms::DaemonResponse::SetBatteryHealthOptimizer { result } = result {
                match result {
                    true => {
                        println!("Successfully turned off bho");
                    }
                    false => {
                        eprintln!("Failed to turn off bho");
                    }
                }
            }
        },
    );
}

fn send_standard_effect(name: String, params: Vec<u8>) {
    match send_data(comms::DaemonCommand::SetStandardEffect { name, params }) {
        Some(comms::DaemonResponse::SetStandardEffect { result }) => {
            if result {
                println!("Effect set OK!");
            } else {
                eprintln!("Effect set FAIL!");
            }
        },
        Some(_) => eprintln!("Unexpected response from daemon!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

fn send_effect(name: String, params: Vec<u8>) {
    match send_data(comms::DaemonCommand::SetEffect { name, params }) {
        Some(comms::DaemonResponse::SetEffect { result }) => {
            if result {
                println!("Effect set OK!");
            } else {
                eprintln!("Effect set FAIL!");
            }
        },
        Some(_) => eprintln!("Unexpected response from daemon!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

fn send_data(opt: comms::DaemonCommand) -> Option<comms::DaemonResponse> {
    match comms::bind() {
        Some(socket) => comms::send_to_daemon(opt, socket),
        None => {
            eprintln!("Error. Cannot bind to socket");
            None
        },
    }
}

fn read_fan_rpm(ac: usize) {
    match send_data(comms::DaemonCommand::GetFanSpeed { ac }) {
        Some(comms::DaemonResponse::GetFanSpeed { rpm }) => {
            let rpm_desc: String = match rpm {
                f if f < 0 => String::from("Unknown"),
                0 => String::from("Auto (0)"),
                _ => format!("{} RPM", rpm),
            };
            println!("Current fan setting: {}", rpm_desc);
        },
        Some(_) => eprintln!("Daemon responded with invalid data!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

fn read_actual_fan_rpm() {
    match send_data(comms::DaemonCommand::GetActualFanRpm) {
        Some(comms::DaemonResponse::GetActualFanRpm { rpm }) => {
            println!("{}", rpm);
        },
        Some(_) => eprintln!("Daemon responded with invalid data!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

fn read_logo_mode(ac: usize) {
    match send_data(comms::DaemonCommand::GetLogoLedState { ac }) {
        Some(comms::DaemonResponse::GetLogoLedState { logo_state }) => {
            let logo_state_desc: &str = match logo_state {
                0 => "Off",
                1 => "On",
                2 => "Breathing",
                _ => "Unknown",
            };
            println!("Current logo setting: {}", logo_state_desc);
        },
        Some(_) => eprintln!("Daemon responded with invalid data!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

fn read_power_mode(ac: usize) {
    if let Some(resp) = send_data(comms::DaemonCommand::GetPwrLevel { ac }) {
        if let comms::DaemonResponse::GetPwrLevel { pwr } = resp {
            let power_desc: &str = match pwr {
                0 => "Balanced (AC)",
                2 => "Performance",
                3 => "Battery Saver",
                4 => "Custom",
                5 => "Silent",
                6 => "Balanced (battery)",
                1 => "Legacy Gaming (hidden)",
                7 => "HyperBoost / cooling-pad (UNSAFE)",
                _ => "Unknown",
            };
            println!("Current power setting: {}", power_desc);
            if pwr == 4 {
                if let Some(resp) = send_data(comms::DaemonCommand::GetCPUBoost { ac }) {
                    if let comms::DaemonResponse::GetCPUBoost { cpu } = resp {
                        let cpu_boost_desc: &str = match cpu {
                            0 => "Low",
                            1 => "Medium",
                            2 => "High",
                            _ => "Unknown",
                        };
                        println!("Current CPU setting: {}", cpu_boost_desc);
                    };
                }
                if let Some(resp) = send_data(comms::DaemonCommand::GetGPUBoost { ac }) {
                    if let comms::DaemonResponse::GetGPUBoost { gpu } = resp {
                        let gpu_boost_desc: &str = match gpu {
                            0 => "Low",
                            1 => "Medium",
                            2 => "High",
                            _ => "Unknown",
                        };
                        println!("Current GPU setting: {}", gpu_boost_desc);
                    };
                }
            }
        } else {
            eprintln!("Daemon responded with invalid data!");
        }
    }
}

fn write_pwr_mode(ac: usize, profile: ProfileArg, cpu_mode: Option<u8>, gpu_mode: Option<u8>) {
    // Resolve the profile to its EC wire value for this domain. An out-of-domain
    // profile (e.g. performance on battery, battery-saver on AC) is rejected —
    // this is where the Synapse AC/DC partition is enforced.
    let pwr = match profile.wire_value(ac) {
        Some(v) => v,
        None => {
            let (domain, allowed) = if ac == 1 {
                ("AC (plugged in)", "balanced, performance, silent, custom")
            } else {
                ("battery (DC)", "balanced, battery-saver")
            };
            Cli::command()
                .error(
                    ErrorKind::InvalidValue,
                    format!("That profile is not available on {domain}. Allowed here: {allowed}."),
                )
                .exit()
        },
    };

    // Boost presets apply to Custom only; the scale is 0=low, 1=medium, 2=high.
    let (cm, gm) = if profile.is_custom() {
        let missing = || {
            Cli::command()
                .error(
                    ErrorKind::MissingRequiredArgument,
                    "custom requires a CPU and a GPU boost (0, 1 or 2), e.g. `write power ac custom 2 0`",
                )
                .exit()
        };
        let cm: u8 = cpu_mode.unwrap_or_else(&missing);
        let gm: u8 = gpu_mode.unwrap_or_else(&missing);
        if cm > 2 || gm > 2 {
            Cli::command()
                .error(ErrorKind::InvalidValue, "CPU/GPU boost must be 0 (low), 1 (medium) or 2 (high)")
                .exit()
        }
        (cm, gm)
    } else {
        if cpu_mode.is_some() || gpu_mode.is_some() {
            Cli::command()
                .error(ErrorKind::InvalidValue, "CPU/GPU boost apply only to the custom profile")
                .exit()
        }
        (0, 0)
    };

    match send_data(comms::DaemonCommand::SetPowerMode {
        ac,
        pwr,
        cpu: cm,
        gpu: gm,
    }) {
        Some(_) => read_power_mode(ac),
        None => {
            Cli::command()
                .error(
                    ErrorKind::DisplayHelp,
                    "An error occurred while sending the command to the daemon",
                )
                .exit()
        },
    }
}

fn read_brightness(ac: usize) {
    match send_data(comms::DaemonCommand::GetBrightness { ac }) {
        Some(comms::DaemonResponse::GetBrightness { result }) => {
            println!("Current brightness: {}", result);
        },
        Some(_) => eprintln!("Daemon responded with invalid data!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

fn read_sync() {
    match send_data(comms::DaemonCommand::GetSync()) {
        Some(comms::DaemonResponse::GetSync { sync }) => {
            println!("Current sync: {:?}", sync);
        },
        Some(_) => eprintln!("Daemon responded with invalid data!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

fn write_brightness(ac: usize, val: u8) {
    match send_data(comms::DaemonCommand::SetBrightness { ac, val }) {
        Some(_) => read_brightness(ac),
        None => eprintln!("Unknown error!"),
    }
}

fn write_fan_speed(ac: usize, x: i32) {
    match send_data(comms::DaemonCommand::SetFanSpeed { ac, rpm: x }) {
        Some(comms::DaemonResponse::SetFanSpeed { result: false }) => {
            // The daemon logs the concrete reason (range/device) to its journal.
            eprintln!(
                "Daemon rejected fan rpm {} (0 = auto, otherwise the model range; see `journalctl --user -u razercontrol`).",
                x
            );
            std::process::exit(1);
        }
        Some(_) => read_fan_rpm(ac),
        None => eprintln!("Unknown error!"),
    }
}

fn write_logo_mode(ac: usize, x: u8) {
    match send_data(comms::DaemonCommand::SetLogoLedState { ac, logo_state: x }) {
        Some(_) => read_logo_mode(ac),
        None => eprintln!("Unknown error!"),
    }
}

fn write_sync(sync: bool) {
    match send_data(comms::DaemonCommand::SetSync { sync }) {
        Some(_) => read_sync(),
        None => eprintln!("Unknown error!"),
    }
}

fn read_gpu_status() {
    match send_data(comms::DaemonCommand::GetGpuStatus) {
        Some(comms::DaemonResponse::GetGpuStatus { gpus, dgpu_runtime_pm }) => {
            println!("Detected GPUs:");
            for gpu in &gpus {
                let type_label = if gpu.gpu_type == "dgpu" { "dGPU" } else { "iGPU" };
                println!("  {} [{}] {} (driver: {}, status: {})", type_label, gpu.pci_slot, gpu.name, gpu.driver, gpu.runtime_status);
            }
            println!("dGPU Runtime PM: {}", if dgpu_runtime_pm { "auto (power saving)" } else { "on (always active)" });
        },
        Some(_) => eprintln!("Daemon responded with invalid data!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

fn source_label(source: comms::CurveTempSource) -> &'static str {
    match source {
        comms::CurveTempSource::Cpu => "CPU",
        comms::CurveTempSource::Gpu => "GPU",
        comms::CurveTempSource::Both => "Both (higher resulting RPM)",
    }
}

fn format_points(points: &[comms::FanCurvePoint]) -> String {
    points
        .iter()
        .map(|p| format!("{}\u{00B0}C:{}", p.temp_c, p.rpm))
        .collect::<Vec<_>>()
        .join(", ")
}

fn get_fan_curve(ac: usize) -> Option<comms::FanCurve> {
    match send_data(comms::DaemonCommand::GetFanCurve { ac }) {
        Some(comms::DaemonResponse::GetFanCurve { curve }) => Some(curve),
        Some(_) => {
            eprintln!("Daemon responded with invalid data!");
            None
        }
        None => {
            eprintln!("Unknown daemon error!");
            None
        }
    }
}

fn read_fan_curve(ac: usize) {
    if let Some(curve) = get_fan_curve(ac) {
        println!("Smart fan curve: {}", if curve.enabled { "enabled" } else { "disabled" });
        println!("Temperature source: {}", source_label(curve.source));
        println!("CPU points: {}", format_points(&curve.cpu_points));
        println!("GPU points: {}", format_points(&curve.gpu_points));
    }
}

/// Parse "40:2200,60:3000,80:4500" into curve points, validating that
/// temperatures are 0..=100 and strictly ascending.
fn parse_points(raw: &str) -> Result<Vec<comms::FanCurvePoint>, String> {
    let mut points: Vec<comms::FanCurvePoint> = Vec::new();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (temp_str, rpm_str) = pair
            .split_once(':')
            .ok_or_else(|| format!("Invalid point '{}' (expected temp:rpm)", pair))?;
        let temp_c: u8 = temp_str
            .trim()
            .parse()
            .map_err(|_| format!("Invalid temperature '{}'", temp_str))?;
        let rpm: u16 = rpm_str
            .trim()
            .parse()
            .map_err(|_| format!("Invalid RPM '{}'", rpm_str))?;
        if temp_c > 100 {
            return Err(format!("Temperature {} out of range (0-100)", temp_c));
        }
        if let Some(last) = points.last() {
            if temp_c <= last.temp_c {
                return Err("Points must be sorted by ascending temperature".to_string());
            }
        }
        points.push(comms::FanCurvePoint { temp_c, rpm });
    }
    if points.is_empty() {
        return Err("No valid points provided".to_string());
    }
    Ok(points)
}

fn write_fan_curve(params: FanCurveParams) {
    let ac = params.ac_state.as_index();
    let mut curve = match get_fan_curve(ac) {
        Some(c) => c,
        None => return,
    };

    if let Some(source) = params.source {
        curve.source = source.to_source();
    }
    if params.enable {
        curve.enabled = true;
    }
    if params.disable {
        curve.enabled = false;
    }
    if let Some(raw) = params.cpu_points.as_deref() {
        match parse_points(raw) {
            Ok(points) => curve.cpu_points = points,
            Err(e) => Cli::command().error(ErrorKind::InvalidValue, e).exit(),
        }
    }
    if let Some(raw) = params.gpu_points.as_deref() {
        match parse_points(raw) {
            Ok(points) => curve.gpu_points = points,
            Err(e) => Cli::command().error(ErrorKind::InvalidValue, e).exit(),
        }
    }

    match send_data(comms::DaemonCommand::SetFanCurve { ac, curve }) {
        Some(comms::DaemonResponse::SetFanCurve { result }) => {
            if result {
                read_fan_curve(ac);
            } else {
                eprintln!("Failed to save fan curve");
            }
        }
        Some(_) => eprintln!("Daemon responded with invalid data!"),
        None => eprintln!("Unknown daemon error!"),
    }
}

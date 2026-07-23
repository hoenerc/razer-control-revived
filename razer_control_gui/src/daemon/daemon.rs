use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time;

use log::*;
use signal_hook::iterator::Signals;
use signal_hook::consts::{SIGINT, SIGTERM};
use dbus::blocking::Connection;
use dbus::{Message, arg};

#[path = "../comms.rs"]
mod comms;
mod config;
mod device;
mod gpu;
mod battery;
mod login1;
mod powerkey;


// The dGPU's power zone (custom-mode GPU boost / TGP) only latches while the
// dGPU is runtime-active. At boot — and any time no GPU client is running — the
// dGPU is runtime-suspended, so the profile applied at startup does not stick
// for the GPU; a game then wakes the dGPU at the balanced TGP. Re-applying the
// profile each time the dGPU resumes makes custom-mode GPU boost take effect.
const DGPU_RESUME_POLL_SECS: u64 = 2;

// On system resume the laptop firmware resets the GPU power zone to its default,
// and may finish that reset several seconds after the wake signal — so a single
// post-wake re-apply can fire too early and lose the race, leaving the dGPU at
// the balanced TGP. A game running across the suspend keeps the dGPU active, so
// the suspended->active watcher never fires either. Re-asserting the profile a
// few times across a settling window re-latches custom-mode GPU boost whenever
// the firmware finishes its reset.
//
// send_report now confirms each write against the EC (busy-poll until success),
// so a re-apply that lands is known to have landed and this no longer needs to
// brute-force comms reliability; the remaining repeats only cover the firmware
// finishing its GPU-power-zone reset a few seconds into the settling window.
const WAKE_SETTLE_REAPPLIES: u32 = 3;
const WAKE_SETTLE_INTERVAL_SECS: u64 = 2;

// A single re-apply when the dGPU first goes active can lose the same race the
// wake path guards against: after a system resume the firmware may still be
// finishing its GPU-power-zone reset when a game wakes the dGPU, overwriting a
// one-shot re-apply back to the balanced TGP. Re-asserting the profile across
// the next few poll ticks — but only while the dGPU stays active — re-latches
// custom-mode GPU boost once the firmware has settled. With confirmed writes
// (see send_report) this only needs to span the firmware's settle window, not
// compensate for dropped commands.
const DGPU_RESUME_REAPPLIES: u32 = 3;

// How often the smart fan-curve control loop re-evaluates temperatures and
// drives the fans. A step lookup plus last-value equality keeps this from
// hunting at steady state, so a short cadence is safe.
const FAN_CURVE_POLL_SECS: u64 = 2;

// Process-lifetime singletons. std::sync::LazyLock (stable since Rust 1.80)
// replaces the former lazy_static dependency with identical semantics:
// initialised on first access, then a plain &'static Mutex<T>.

static DEV_MANAGER: std::sync::LazyLock<Mutex<device::DeviceManager>> =
    std::sync::LazyLock::new(|| match device::DeviceManager::read_laptops_file() {
        Ok(c) => Mutex::new(c),
        Err(_) => Mutex::new(device::DeviceManager::new()),
    });

// Main function for daemon
fn main() {
    setup_panic_hook();
    init_logging();

    if let Ok(mut d) = DEV_MANAGER.lock() {
        d.discover_devices();
        if let Some(laptop) = d.get_device() {
            println!("supported device: {:?}", laptop.get_name());
        } else {
            println!("no supported device found");
            std::process::exit(1);
        }
    } else {
        println!("error loading supported devices");
        std::process::exit(1);
    }


    if let Ok(mut d) = DEV_MANAGER.lock() {
        let dbus_system = match Connection::new_system() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Failed to connect to D-Bus system bus: {}", e);
                std::process::exit(1);
            }
        };
        let proxy_ac = dbus_system.with_proxy("org.freedesktop.UPower", "/org/freedesktop/UPower/devices/line_power_AC0", time::Duration::from_millis(5000));
        use battery::OrgFreedesktopUPowerDevice;
        if let Ok(online) = proxy_ac.online() {
            println!("Online AC0: {:?}", online);
            // Seed the mirrors only (lighting restore below reads them); the
            // single profile apply comes from the charger-aware sync — the
            // AC0 flag cannot tell barrel from PD, and PD must land on Balanced
            // without the stored AC profile flashing onto the EC first.
            d.set_ac_mirror(online);
            if !d.restore_static_color() {
                eprintln!("static colour restore failed at startup — re-applies on the next enable or Apply");
            }
            d.restore_bho();
            d.sync_charger_domain();
        } else {
            println!("error getting current power state");
            std::process::exit(1);
        }
    }

    start_battery_monitor_task();
    start_dgpu_resume_watch_task();
    start_fan_curve_task();
    powerkey::start_power_key_task();
    let clean_thread = start_shutdown_task();

    if let Some(listener) = comms::create() {
        // Err(_): don't care about this
        for stream in listener.incoming().flatten() {
            handle_data(stream)
        }
    } else {
        eprintln!("Could not create Unix socket!");
        std::process::exit(1);
    }
    clean_thread.join().unwrap();
}

/// Installs a custom panic hook to perform cleanup when the daemon crashes
fn setup_panic_hook() {
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!("Something went wrong! Removing the socket path");
        let _ = std::fs::remove_file(comms::socket_path());
        default_panic_hook(info);
    }));
}

fn init_logging() {
    let mut builder = env_logger::Builder::from_default_env();
    builder.target(env_logger::Target::Stderr);
    builder.filter_level(log::LevelFilter::Info);
    builder.format_timestamp_millis();
    builder.parse_env("RAZER_LAPTOP_CONTROL_LOG");
    builder.init();
}

fn start_battery_monitor_task() -> JoinHandle<()> {
    thread::spawn(move || {
        let dbus_system = match Connection::new_system() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Battery monitor: D-Bus system unavailable ({}), skipping", e);
                return;
            }
        };
        let proxy_ac = dbus_system.with_proxy("org.freedesktop.UPower", "/org/freedesktop/UPower/devices/line_power_AC0", time::Duration::from_millis(5000));
        let _id = proxy_ac.match_signal(|h: battery::OrgFreedesktopDBusPropertiesPropertiesChanged, _: &Connection, _: &Message| {
            let online: Option<&bool> = arg::prop_cast(&h.changed_properties, "Online");
            if let Some(online) = online {
                println!("Online AC0: {:?}", online);
                if let Ok(mut d) = DEV_MANAGER.lock() {
                    // Mirror only, then ONE charger-aware apply. This event
                    // fires for barrel<->battery and PD<->battery, but NOT for
                    // a barrel<->PD hot-swap (both keep AC0 online=1) — that
                    // case is healed by the power key.
                    d.set_ac_mirror(*online);
                    d.sync_charger_domain();
                }
            }
            true
        });

        let proxy_login = dbus_system.with_proxy("org.freedesktop.login1", "/org/freedesktop/login1", time::Duration::from_millis(5000));
        let _id = proxy_login.match_signal(|h: login1::OrgFreedesktopLogin1ManagerPrepareForSleep, _: &Connection, _: &Message| {
            println!("PrepareForSleep {:?}", h.start);
            if let Ok(mut d) = DEV_MANAGER.lock() {
                if h.start {
                    d.light_off();
                } else {
                    d.restore_light();
                    // v2.14.1: the unconditional binary set_ac_state_get() that
                    // used to run here restored the FULL stored AC config on
                    // every wake before anyone had looked at the charger — a
                    // transient stored-AC write under PD on each resume.
                    // Resolve the domain first; the settle passes below stay.
                    d.sync_charger_domain();

                    // The system just woke up. UPower can be slow to update its AC state, and the
                    // firmware resets the GPU power zone during resume and may finish that reset
                    // seconds later. Re-read AC and re-apply the profile across a settling window so
                    // the correct profile re-latches whenever both have settled.
                    thread::spawn(|| {
                        for _ in 0..WAKE_SETTLE_REAPPLIES {
                            thread::sleep(time::Duration::from_secs(WAKE_SETTLE_INTERVAL_SECS));
                            if let Ok(mut dev) = DEV_MANAGER.lock() {
                                println!("Post-wake re-apply (settling)");
                                // Charger-aware: the settle window is also where
                                // the first post-plug 0x8c read stops echoing the
                                // stale value, so the last pass lands correct.
                                dev.sync_charger_domain();
                            }
                        }
                    });
                }
            }
            true
        });
        // use login1::OrgFreedesktopLogin1ManagerPrepareForSleep;
        loop {
            if let Err(e) = dbus_system.process(time::Duration::from_millis(1000)) {
                eprintln!("Battery monitor D-Bus error: {}", e);
            }
        }
    })
}

/// Re-applies the saved power profile whenever the dGPU transitions from
/// runtime-suspended to active, so custom-mode GPU boost latches once a GPU
/// client (e.g. a game) powers the dGPU up. Each transition starts a settling
/// burst (re-asserting on the next few poll ticks while the dGPU stays active)
/// so a late post-resume firmware reset cannot leave the dGPU at the balanced
/// TGP. See DGPU_RESUME_POLL_SECS and DGPU_RESUME_REAPPLIES.
fn start_dgpu_resume_watch_task() -> JoinHandle<()> {
    thread::spawn(|| {
        let mut dgpu_path = gpu::find_dgpu_sysfs_path();
        let mut was_active = false;
        let mut reapplies_remaining: u32 = 0;
        loop {
            thread::sleep(time::Duration::from_secs(DGPU_RESUME_POLL_SECS));
            if dgpu_path.is_none() {
                dgpu_path = gpu::find_dgpu_sysfs_path();
            }
            let active = dgpu_path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p.join("power/runtime_status")).ok())
                .is_some_and(|s| s.trim() == "active");
            if active && !was_active {
                println!("dGPU resumed — re-applying power profile (settling)");
                reapplies_remaining = DGPU_RESUME_REAPPLIES;
            }
            if active && reapplies_remaining > 0 {
                if let Ok(mut d) = DEV_MANAGER.lock() {
                    d.reapply_power_mode();
                }
                reapplies_remaining -= 1;
            }
            was_active = active;
        }
    })
}

/// Drives the fans from the user's smart fan curve when one is enabled for the
/// current AC state. Each tick it reads only the temperatures the active source
/// needs, then hands them to the device manager which performs the step lookup
/// and applies the result. See FAN_CURVE_POLL_SECS.
fn start_fan_curve_task() -> JoinHandle<()> {
    thread::spawn(|| {
        loop {
            thread::sleep(time::Duration::from_secs(FAN_CURVE_POLL_SECS));

            // Decide which temps to read without holding the lock across the
            // (potentially slow) sensor reads.
            let source = match DEV_MANAGER.lock() {
                Ok(mut d) => d.active_fan_curve_source(),
                Err(_) => continue,
            };
            let source = match source {
                Some(s) => s,
                None => continue, // no curve enabled for the current AC state
            };

            use comms::CurveTempSource::*;
            let cpu_temp = match source {
                Cpu | Both => read_cpu_temperature(),
                Gpu => None,
            };
            let gpu_temp = match source {
                Gpu | Both => sample_dgpu_sensors(),
                Cpu => None,
            };

            if let Ok(mut d) = DEV_MANAGER.lock() {
                d.fan_curve_tick(cpu_temp, gpu_temp);
            }
        }
    })
}

/// Read CPU temperature in °C from hwmon (AMD k10temp/zenpower or Intel coretemp).
/// The matching temp1_input path is cached after the first successful scan:
/// hwmon numbering is stable within a boot and this runs on every curve tick,
/// so the cache turns a per-tick directory sweep into one file read. A failed
/// read on the cached path (driver reload edge case) clears the cache and the
/// next tick rescans instead of failing forever.
fn read_cpu_temperature() -> Option<f64> {
    use std::sync::{Mutex, OnceLock};
    static CPU_TEMP_PATH: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();
    let cache = CPU_TEMP_PATH.get_or_init(|| Mutex::new(None));

    let cached = cache.lock().ok()?.clone();
    if let Some(path) = cached {
        if let Some(temp) = read_temp_milli(&path) {
            return Some(temp);
        }
        if let Ok(mut slot) = cache.lock() {
            *slot = None;
        }
    }

    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in entries.flatten() {
        let name = match std::fs::read_to_string(entry.path().join("name")) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let name = name.trim();
        if name == "k10temp" || name == "zenpower" || name == "coretemp" {
            let path = entry.path().join("temp1_input");
            if let Some(temp) = read_temp_milli(&path) {
                if let Ok(mut slot) = cache.lock() {
                    *slot = Some(path);
                }
                return Some(temp);
            }
        }
    }
    None
}

fn read_temp_milli(path: &std::path::Path) -> Option<f64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|milli| milli / 1000.0)
}

// Freshness ceiling for the dGPU sensor cache. Entries older than this are
// treated as absent, so a client can never display values from a sampling
// session that has ended (curve disabled, source switched to Cpu, dGPU
// suspended, game closed). Two curve ticks plus slack.
const DGPU_SENSOR_MAX_AGE_MS: u64 = 10_000;

/// Last dGPU sensor snapshot: (temp °C, power W, util %, sampled-at).
/// Written exclusively by the fan-curve task; read via GetDgpuSensors.
type DgpuSensorSample = (f64, Option<f64>, Option<u32>, time::Instant);
static DGPU_SENSOR_CACHE: std::sync::LazyLock<Mutex<Option<DgpuSensorSample>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// Sample dGPU (NVIDIA) temperature/power/utilization and refresh the sensor
/// cache; returns the temperature in °C (the only value the curve itself
/// needs). Returns None when the dGPU is runtime-suspended so we never spin it
/// up just to read a sensor — an idle dGPU has no thermal load driving the
/// fans anyway.
///
/// Uses nvidia-smi deliberately: the NVIDIA driver (open modules included)
/// exposes no hwmon node, so there is no sysfs alternative. This is the sole
/// nvidia-smi call site in the ENTIRE project, daemon and GUI included — the
/// GUI displays dGPU sensors exclusively from this cache via GetDgpuSensors
/// and never spawns nvidia-smi itself. One process spawn fetches all three
/// fields; power/utilization ride along at zero extra GSP-RPC cost purely so
/// the GUI monitor has them. The call is doubly conditional — the fan-curve
/// task only invokes it while a smart curve with a GPU/Both source is enabled
/// for the active power domain, and the guard below ensures it only runs
/// while the dGPU is already awake. With smart curves disabled (or a Cpu-only
/// source) it never executes and the cache simply ages out.
fn sample_dgpu_sensors() -> Option<f64> {
    let dgpu_active = gpu::find_dgpu_sysfs_path()
        .and_then(|p| std::fs::read_to_string(p.join("power/runtime_status")).ok())
        .is_some_and(|s| s.trim() == "active");
    if !dgpu_active {
        return None;
    }

    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu,power.draw,utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let fields: Vec<&str> = stdout.trim().split(',').map(str::trim).collect();
    // temperature.gpu must parse; power.draw and utilization.gpu may report
    // "[N/A]" in some driver states and are therefore optional.
    let temp = fields.first()?.parse::<f64>().ok()?;
    let power = fields.get(1).and_then(|s| s.parse::<f64>().ok());
    let util = fields.get(2).and_then(|s| s.parse::<u32>().ok());

    if let Ok(mut cache) = DGPU_SENSOR_CACHE.lock() {
        *cache = Some((temp, power, util, time::Instant::now()));
    }
    Some(temp)
}

/// Snapshot for GetDgpuSensors: the cached values with their age, or None when
/// nothing fresh exists (no GPU/Both curve sampling, dGPU asleep, aged out).
fn dgpu_sensor_snapshot() -> Option<comms::DgpuSensors> {
    let cache = DGPU_SENSOR_CACHE.lock().ok()?;
    let (temp, power, util, at) = (*cache)?;
    let age_ms = at.elapsed().as_millis() as u64;
    if age_ms > DGPU_SENSOR_MAX_AGE_MS {
        return None;
    }
    Some(comms::DgpuSensors {
        temp_c: temp,
        power_w: power,
        util_pct: util,
        age_ms,
    })
}

/// Monitors signals and stops the daemon when receiving one
pub fn start_shutdown_task() -> JoinHandle<()> {
    thread::spawn(|| {
        let mut signals = Signals::new([SIGINT, SIGTERM]).unwrap();
        let _ = signals.forever().next();
        
        // If we reach this point, we have a signal and it is time to exit
        println!("Received signal, cleaning up");
        let _ = std::fs::remove_file(comms::socket_path());
        std::process::exit(0);
    })
}

fn handle_data(mut stream: UnixStream) {
    // The accept loop is single-threaded and read_to_end blocks until EOF: a
    // client that connects and never shuts down its write side used to park
    // the ENTIRE command path (powerkey included) forever. 2 s is generous —
    // clients write one small bincode command and shutdown immediately.
    let _ = stream.set_read_timeout(Some(time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(time::Duration::from_secs(2)));

    // Largest legitimate command (SetFanCurve) is well under a kilobyte; cap
    // the read so a broken client cannot balloon daemon memory.
    let mut buffer = Vec::new();
    if let Err(error) = (&mut stream).take(64 * 1024).read_to_end(&mut buffer) {
        eprintln!("Failed to read request from socket: {error}");
        return;
    }

    if buffer.is_empty() {
        eprintln!("Received empty request payload");
        return;
    }

    if let Some(cmd) = comms::read_from_socket_req(&buffer) {
        if let Some(s) = process_client_request(cmd) {
            if let Ok(x) = bincode::serialize(&s) {
                let result = stream.write_all(&x);

                if let Err(error) = result {
                    println!("Client disconnected with error: {error}");
                }
            }
        } else {
            eprintln!("No response for client request — closing connection");
        }
    } else {
        eprintln!("Failed to deserialize client request");
    }
}

pub fn process_client_request(cmd: comms::DaemonCommand) -> Option<comms::DaemonResponse> {
    // State-changing commands get an info-level journal line. The debug-level
    // diet silenced REQ/RES entirely — correct for the 2 s Get polling, but it
    // also made GUI/CLI-initiated switches invisible during verification
    // (powerkey cycles and profile reapplies log, so sets must too, or the
    // journal tells only half the story of how the EC reached its state).
    // Gets stay silent; sets are rare, deliberate events.
    match &cmd {
        // Compact one-liner for curves — the full Debug dump (twelve points,
        // one wrapped ~700-char journal line per curve edit) drowned the log.
        comms::DaemonCommand::SetFanCurve { ac, curve } => {
            let fmt = |pts: &[comms::FanCurvePoint]| pts
                .iter()
                .map(|p| format!("{}→{}", p.temp_c, p.rpm))
                .collect::<Vec<_>>()
                .join(" ");
            println!(
                "state change: SetFanCurve {{ ac: {}, enabled: {}, source: {:?}, cpu: [{}], gpu: [{}] }}",
                ac, curve.enabled, curve.source,
                fmt(&curve.cpu_points), fmt(&curve.gpu_points)
            );
        }
        comms::DaemonCommand::SetPowerMode { .. }
        | comms::DaemonCommand::CyclePowerMode
        | comms::DaemonCommand::SetFanSpeed { .. }
        | comms::DaemonCommand::SetExperimentalProfiles { .. }
        | comms::DaemonCommand::SetBatteryHealthOptimizer { .. }
        | comms::DaemonCommand::SetBrightness { .. }
        | comms::DaemonCommand::SetLogoLedState { .. }
        | comms::DaemonCommand::SetStaticColor { .. }
        | comms::DaemonCommand::SetStaticLighting { .. } => {
            println!("state change: {:?}", cmd);
        }
        _ => {}
    }

    // GPU commands don't need DEV_MANAGER, handle them first
    match &cmd {
        comms::DaemonCommand::GetGpuStatus => {
            // Read-only status for `razer-cli read gpu` and dGPU detection:
            // lspci/sysfs only, never wakes the dGPU. envycontrol mode
            // switching and the dGPU-suspend toggle were removed in v2.8 —
            // both needed root writes a user daemon cannot perform, so the
            // controls never functioned; runtime-PM policy belongs to the
            // distro's udev rules, where D3cold demonstrably works already.
            return Some(comms::DaemonResponse::GetGpuStatus {
                gpus: gpu::discover_gpus(),
                dgpu_runtime_pm: gpu::get_dgpu_runtime_pm(),
            });
        }
        comms::DaemonCommand::GetDgpuSensors => {
            // Pure cache read: never touches the GPU, the driver, or the EC.
            return Some(comms::DaemonResponse::GetDgpuSensors {
                sensors: dgpu_sensor_snapshot(),
            });
        }
        comms::DaemonCommand::SetExperimentalProfiles { enabled } => {
            let result = DEV_MANAGER
                .lock()
                .map(|mut d| d.set_experimental_profiles(*enabled))
                .unwrap_or(false);
            return Some(comms::DaemonResponse::SetExperimentalProfiles { result });
        }
        comms::DaemonCommand::GetExperimentalProfiles => {
            let enabled = DEV_MANAGER
                .lock()
                .ok()
                .and_then(|d| d.config.as_ref().map(|c| c.experimental_profiles))
                .unwrap_or(false);
            return Some(comms::DaemonResponse::GetExperimentalProfiles { enabled });
        }
        _ => {}
    }

    if let Ok(mut d) = DEV_MANAGER.lock() {
        match cmd {
            comms::DaemonCommand::SetPowerMode { ac, pwr, cpu, gpu } if ac < 2 => {
                Some(comms::DaemonResponse::SetPowerMode { result: d.set_power_mode(ac, pwr, cpu, gpu) })
            },
            comms::DaemonCommand::SetFanSpeed { ac, rpm } if ac < 2 => {
                Some(comms::DaemonResponse::SetFanSpeed { result: d.set_fan_rpm(ac, rpm) })
            },
            comms::DaemonCommand::SetLogoLedState{ ac, logo_state } if ac < 2 => {
                Some(comms::DaemonResponse::SetLogoLedState { result: d.set_logo_led_state(ac, logo_state) })
            },
            comms::DaemonCommand::SetBrightness { ac, val } if ac < 2 => {
                Some(comms::DaemonResponse::SetBrightness {result: d.set_brightness(ac, val) })
            }
            comms::DaemonCommand::GetBrightness{ac} if ac < 2 =>  {
                Some(comms::DaemonResponse::GetBrightness { result: d.get_brightness(ac)})
            },
            comms::DaemonCommand::GetLogoLedState{ac} if ac < 2 => Some(comms::DaemonResponse::GetLogoLedState {logo_state: d.get_logo_led_state(ac) }),
            comms::DaemonCommand::SetStaticColor { red, green, blue } => {
                Some(comms::DaemonResponse::SetStaticColor { result: d.set_static_color([red, green, blue]) })
            }
            comms::DaemonCommand::GetStaticColor => {
                Some(comms::DaemonResponse::GetStaticColor { color: d.get_static_color() })
            }
            comms::DaemonCommand::SetStaticLighting { enabled } => {
                Some(comms::DaemonResponse::SetStaticLighting { result: d.set_static_lighting(enabled) })
            }
            comms::DaemonCommand::GetStaticLighting => {
                Some(comms::DaemonResponse::GetStaticLighting { enabled: d.get_static_lighting() })
            }
            comms::DaemonCommand::GetCapabilities { ac } if ac < 2 => {
                let (wires, max_boost_tier, model) = d.get_capabilities(ac);
                Some(comms::DaemonResponse::GetCapabilities { wires, max_boost_tier, model })
            }
            comms::DaemonCommand::GetFanSpeed{ac} if ac < 2 => Some(comms::DaemonResponse::GetFanSpeed { rpm: d.get_fan_rpm(ac)}),
            comms::DaemonCommand::GetPwrLevel{ac} if ac < 2 => Some(comms::DaemonResponse::GetPwrLevel { pwr: d.get_power_mode(ac) }),
            comms::DaemonCommand::GetCharger => {
                let actp = d.read_charger_domain_raw();
                Some(comms::DaemonResponse::GetCharger { actp })
            },
            comms::DaemonCommand::GetDesiredState => {
                let state = d.desired_state_wire();
                Some(comms::DaemonResponse::GetDesiredState { state })
            },
            comms::DaemonCommand::GetEcPowerZone { zone } if (1..=2).contains(&zone) => {
                let mode = d.ec_power_zone(zone);
                Some(comms::DaemonResponse::GetEcPowerZone { mode })
            },
            comms::DaemonCommand::GetEcBoost { gpu } => {
                let value = d.ec_boost(gpu);
                Some(comms::DaemonResponse::GetEcBoost { value })
            },
            comms::DaemonCommand::GetEcBrightness => {
                let value = d.ec_brightness();
                Some(comms::DaemonResponse::GetEcBrightness { value })
            },
            comms::DaemonCommand::GetEcBho => {
                let state = d.ec_bho();
                Some(comms::DaemonResponse::GetEcBho { state })
            },
            comms::DaemonCommand::GetEcFanTach { zone } if (1..=2).contains(&zone) => {
                let rpm = d.ec_fan_tach(zone);
                Some(comms::DaemonResponse::GetEcFanTach { rpm })
            },
            comms::DaemonCommand::GetEcFanSetpoint { zone } if (1..=2).contains(&zone) => {
                let rpm = d.ec_fan_setpoint(zone);
                Some(comms::DaemonResponse::GetEcFanSetpoint { rpm })
            },
            comms::DaemonCommand::CyclePowerMode => {
                let applied = d.cycle_power_key().map(|(wire, domain, cold)| {
                    comms::CycleResult { wire, domain: domain.to_wire(), cold }
                });
                Some(comms::DaemonResponse::CyclePowerMode { applied })
            },
            comms::DaemonCommand::GetCPUBoost{ac} if ac < 2 => Some(comms::DaemonResponse::GetCPUBoost { cpu: d.get_cpu_boost(ac) }),
            comms::DaemonCommand::GetGPUBoost{ac} if ac < 2 => Some(comms::DaemonResponse::GetGPUBoost { gpu: d.get_gpu_boost(ac) }),
            comms::DaemonCommand::SetBatteryHealthOptimizer { is_on, threshold } => { 
                Some(comms::DaemonResponse::SetBatteryHealthOptimizer { result: d.set_bho_handler(is_on, threshold)})
            }
            comms::DaemonCommand::GetBatteryHealthOptimizer() => {
                d.get_bho_handler().map(|result| 
                    comms::DaemonResponse::GetBatteryHealthOptimizer {
                        is_on: (result.0), 
                        threshold: (result.1) 
                    }
                )
            }
            comms::DaemonCommand::GetActualFanRpm => {
                Some(comms::DaemonResponse::GetActualFanRpm { rpm: d.get_actual_fan_rpm() })
            },
            comms::DaemonCommand::GetDeviceName => {
                let name = match &d.device {
                    Some(device) => device.get_name(),
                    None => "Unknown Device".into()
                };
                Some(comms::DaemonResponse::GetDeviceName { name })
            }
            comms::DaemonCommand::SetFanCurve { ac, curve } if ac < 2 => {
                Some(comms::DaemonResponse::SetFanCurve { result: d.set_fan_curve(ac, curve) })
            }
            comms::DaemonCommand::GetFanCurve { ac } if ac < 2 => {
                Some(comms::DaemonResponse::GetFanCurve { curve: d.get_fan_curve(ac) })
            }
            // Reject commands with invalid ac index (>= 2)
            _ => {
                eprintln!("Rejected command with invalid ac index: {:?}", cmd);
                None
            }
        }
    } else {
        None
    }
}



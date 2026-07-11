use serde::{Deserialize, Serialize};
use std::{fs, fs::File, io, env};
use std::io::prelude::*;
use crate::comms::FanCurve;

const SETTINGS_FILE: &str = "/razercontrol/daemon.json";
const EFFECTS_FILE: &str = "/razercontrol/effects.json";

#[derive(Serialize, Deserialize, Clone)]
pub struct PowerConfig {
    pub power_mode: u8,
    pub cpu_boost: u8,
    pub gpu_boost: u8,
    pub fan_rpm: i32,
    pub brightness: u8,
    pub logo_state: u8,
    pub screensaver: bool, // turno of keyboard light if screen is blank
    pub idle: u32,
    #[serde(default = "FanCurve::new")]
    pub fan_curve: FanCurve,
}

impl PowerConfig {
    pub fn new() -> PowerConfig {
        return PowerConfig{
            power_mode: 0,
            cpu_boost: 1,
            gpu_boost: 0,
            fan_rpm: 0,
            brightness: 128,
            logo_state: 0,
            screensaver: false,
            idle: 0,
            fan_curve: FanCurve::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Configuration {
    pub power: [PowerConfig; 2],
    pub sync: bool, // sync light settings between ac and battery
    pub standard_effect: u8,
    pub standard_effect_params: Vec<u8>,
    #[serde(default)]
    pub bho_on: bool,
    #[serde(default = "default_bho_threshold")]
    pub bho_threshold: u8,
    #[serde(default)]
    pub gui_effect: u8, // GUI custom effect index (0=Static, 1=StaticGradient, 2=WaveGradient, 3=Breathing)
    #[serde(default)]
    pub gui_effect_params: Vec<u8>, // GUI effect color params (RGB bytes)
    /// Experimental unlock (HyperBoost, legacy ghost, boost tier 3).
    /// Default off; toggled in the GUI, enforced daemon-side.
    #[serde(default)]
    pub experimental_profiles: bool,
    /// Config schema version. Pre-v2.9.x files load as 0 via serde default and
    /// are accepted; files from a NEWER schema are quarantined instead of
    /// being silently reinterpreted.
    #[serde(default)]
    pub schema_version: u32,
}

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

fn default_bho_threshold() -> u8 { 80 }

impl Configuration {
    pub fn new() -> Configuration {
        // power[0] is the battery (DC) slot, power[1] the AC slot (see AcState).
        // Default each to its domain's Balanced value on the Blade 16 2025 map:
        // DC Balanced = 6, AC Balanced = 0. The DC slot must not default to an
        // AC wire value or the first battery apply would be non-Synapse-faithful.
        let dc_default = { let mut c = PowerConfig::new(); c.power_mode = 6; c };
        let ac_default = PowerConfig::new(); // power_mode already 0
        return Configuration {
            power: [dc_default, ac_default],
            sync: false,
            standard_effect: 0x04, // spectrum cycling
            standard_effect_params: vec![],
            bho_on: false,
            experimental_profiles: false,
            bho_threshold: 80,
            gui_effect: 0,
            gui_effect_params: vec![],
            schema_version: CONFIG_SCHEMA_VERSION,
        };
    }

    pub fn write_to_file(&mut self) -> io::Result<()> {
        ensure_config_dir()?;
        let j: String = serde_json::to_string_pretty(&self)?;
        write_atomic(&(get_home_directory() + SETTINGS_FILE), j.as_bytes())
    }

    pub fn read_from_config() -> io::Result<Configuration> {
        // "File missing" (first run) and "file unusable" must not look the
        // same: the caller defaults on any Err — right for a first run, but it
        // was silently discarding a damaged or newer-schema file (external
        // review 3.7). Unusable files are quarantined loudly instead.
        let path = get_home_directory() + SETTINGS_FILE;
        match fs::read_to_string(&path) {
            Ok(s) => return parse_or_quarantine(&path, &s),
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            Err(_) => {}
        }
        // New location empty: one-time migration from the pre-XDG path (only
        // ever differs from `path` when XDG_DATA_HOME points elsewhere).
        if let Ok(home) = env::var("HOME") {
            let legacy = home + "/.local/share" + SETTINGS_FILE;
            if legacy != path {
                if let Ok(s) = fs::read_to_string(&legacy) {
                    let cfg = parse_or_quarantine(&legacy, &s)?;
                    ensure_config_dir()?;
                    let j = serde_json::to_string_pretty(&cfg)?;
                    write_atomic(&path, j.as_bytes())?;
                    let parked = format!("{}.migrated", legacy);
                    let _ = fs::rename(&legacy, &parked);
                    eprintln!("config migrated: {} -> {} (original parked as {})", legacy, path, parked);
                    return Ok(cfg);
                }
            }
        }
        Err(io::Error::from(io::ErrorKind::NotFound))
    }

    pub fn write_effects_save(json: serde_json::Value) -> io::Result<()> {
        ensure_config_dir()?;
        let j: String = serde_json::to_string_pretty(&json)?;
        write_atomic(&(get_home_directory() + EFFECTS_FILE), j.as_bytes())
    }

    pub fn read_effects_file() -> io::Result<serde_json::Value> {
        let str = fs::read_to_string(get_home_directory() + EFFECTS_FILE)?;
        let res: serde_json::Value = serde_json::from_str(str.as_str())?;
        Ok(res)
    }
}

/// Crash-safe write: tmp file in the SAME directory (rename must not cross
/// filesystems), fsync, then atomic rename over the target. Previously a crash
/// or power loss mid-write could truncate daemon.json, and the next start fell
/// back to defaults SILENTLY — BHO off, curves gone, custom boosts gone. Every
/// setter writes the config, so this path runs constantly.
fn write_atomic(path: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = format!("{}.tmp", path);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // The rename itself lives in the directory; sync it so a power loss right
    // after the rename cannot resurrect the old file name on some filesystems.
    if let Some(dir) = std::path::Path::new(path).parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

fn parse_or_quarantine(path: &str, contents: &str) -> io::Result<Configuration> {
    match serde_json::from_str::<Configuration>(contents) {
        Ok(cfg) if cfg.schema_version <= CONFIG_SCHEMA_VERSION => Ok(cfg),
        Ok(cfg) => {
            quarantine(path, &format!(
                "schema {} > supported {}", cfg.schema_version, CONFIG_SCHEMA_VERSION
            ));
            Err(io::Error::new(io::ErrorKind::InvalidData, "config from newer schema"))
        }
        Err(e) => {
            quarantine(path, &e.to_string());
            Err(e.into())
        }
    }
}

/// Move a damaged/foreign config aside instead of overwriting it later, and
/// leave one unmissable journal line. The daemon then starts from defaults.
fn quarantine(path: &str, reason: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sidecar = format!("{}.corrupt-{}", path, ts);
    match fs::rename(path, &sidecar) {
        Ok(()) => eprintln!(
            "CONFIG QUARANTINED: {} could not be used ({}) — preserved as {}, starting from defaults",
            path, reason, sidecar
        ),
        Err(e) => eprintln!(
            "CONFIG UNUSABLE: {} ({}) — and quarantine failed too ({}); starting from defaults",
            path, reason, e
        ),
    }
}

/// XDG data directory: $XDG_DATA_HOME, else $HOME/.local/share (the default
/// on this install — existing configs keep resolving to the same path).
fn get_home_directory() -> String {
    if let Ok(xdg) = env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return xdg;
        }
    }
    match env::var("HOME") {
        Ok(home) => home + "/.local/share",
        Err(_) => {
            eprintln!("WARNING: neither XDG_DATA_HOME nor HOME set, falling back to /tmp");
            "/tmp".to_string()
        }
    }
}

fn ensure_config_dir() -> io::Result<()> {
    let dir = get_home_directory() + "/razercontrol";
    fs::create_dir_all(dir)
}

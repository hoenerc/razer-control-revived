use serde::{Deserialize, Serialize};
use std::{fs, fs::File, io, env};
use std::io::prelude::*;
use crate::comms::FanCurve;

const SETTINGS_FILE: &str = "/.local/share/razercontrol/daemon.json";
const EFFECTS_FILE: &str = "/.local/share/razercontrol/effects.json";

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
    pub no_light: f64, // no light bellow this percentage of battery
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
}

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
            no_light: 0.0,
            standard_effect: 0x04, // spectrum cycling
            standard_effect_params: vec![],
            bho_on: false,
            bho_threshold: 80,
            gui_effect: 0,
            gui_effect_params: vec![],
        };
    }

    pub fn write_to_file(&mut self) -> io::Result<()> {
        ensure_config_dir()?;
        let j: String = serde_json::to_string_pretty(&self)?;
        write_atomic(&(get_home_directory() + SETTINGS_FILE), j.as_bytes())
    }

    pub fn read_from_config() -> io::Result<Configuration> {
        let str = fs::read_to_string(get_home_directory() + SETTINGS_FILE)?;
        let res: Configuration = serde_json::from_str(str.as_str())?;
        Ok(res)
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
    fs::rename(&tmp, path)
}

fn get_home_directory() -> String {
    env::var("HOME").unwrap_or_else(|_| {
        eprintln!("WARNING: HOME environment variable not set, falling back to /tmp");
        "/tmp".to_string()
    })
}

fn ensure_config_dir() -> io::Result<()> {
    let dir = get_home_directory() + "/.local/share/razercontrol";
    fs::create_dir_all(dir)
}

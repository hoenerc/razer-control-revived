use serde::{Deserialize, Serialize};
use std::{fs, fs::File, io, env};
use std::io::prelude::*;
use crate::comms::FanCurve;

const SETTINGS_FILE: &str = "/razercontrol/daemon.json";

#[derive(Serialize, Deserialize, Clone)]
pub struct PowerConfig {
    pub power_mode: u8,
    pub cpu_boost: u8,
    pub gpu_boost: u8,
    pub fan_rpm: i32,
    pub brightness: u8,
    pub logo_state: u8,
    #[serde(default = "FanCurve::new")]
    pub fan_curve: FanCurve,
}

impl PowerConfig {
    pub fn new() -> PowerConfig {
        PowerConfig{
            power_mode: 0,
            // Synapse's default with undervolt active — the sanitizer's
            // repair target.
            cpu_boost: 2,
            gpu_boost: 0,
            fan_rpm: 0,
            brightness: 128,
            logo_state: 0,
            fan_curve: FanCurve::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Configuration {
    pub power: [PowerConfig; 2],
    /// The single supported lighting model: one static keyboard colour.
    /// Retired fields (sync, standard_effect*, gui_effect*, per-domain
    /// screensaver/idle) in old files are tolerated — serde ignores unknown
    /// fields — but never written again.
    #[serde(default = "default_static_color")]
    pub static_color: [u8; 3],
    /// Master switch for the lighting scope. When off, the daemon performs
    /// ZERO keyboard-lighting writes (static colour, brightness, logo,
    /// suspend hooks) so external tools like OpenRazer own the hardware
    /// conflict-free. Power/fan/BHO stay fully active. Default on.
    #[serde(default = "default_static_lighting")]
    pub static_lighting: bool,
    #[serde(default)]
    pub bho_on: bool,
    #[serde(default = "default_bho_threshold")]
    pub bho_threshold: u8,
    /// Experimental unlock (HyperBoost, legacy ghost, boost tier 3).
    /// Default off; toggled in the GUI, enforced daemon-side.
    #[serde(default)]
    pub experimental_profiles: bool,
    /// Config schema version. Pre-v2.9.x files load as 0 via serde default,
    /// are accepted and stamped to the current version on load; files from a
    /// NEWER schema are quarantined instead of being silently reinterpreted.
    #[serde(default)]
    pub schema_version: u32,
}

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

fn default_static_color() -> [u8; 3] {
    [0, 255, 0] // Razer green — the historic engine default
}

fn default_static_lighting() -> bool {
    true
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
        Configuration {
            power: [dc_default, ac_default],
            static_color: default_static_color(),
            static_lighting: true,
            bho_on: false,
            experimental_profiles: false,
            bho_threshold: 80,
            schema_version: CONFIG_SCHEMA_VERSION,
        }
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
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                // Bytes exist but are not UTF-8: content-level garbage, the
                // same treatment as unparseable JSON — preserve the file
                // aside; quarantine() leaves the unmissable journal line.
                quarantine(&path, "not valid UTF-8");
                return Err(e);
            }
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
                    match fs::rename(&legacy, &parked) {
                        Ok(()) => eprintln!(
                            "config migrated: {} -> {} (original parked as {})",
                            legacy, path, parked
                        ),
                        Err(e) => eprintln!(
                            "config migrated: {} -> {} (could not park the original as {}: {} — the legacy file stays in place; remove it manually)",
                            legacy, path, parked, e
                        ),
                    }
                    return Ok(cfg);
                }
            }
        }
        Err(io::Error::from(io::ErrorKind::NotFound))
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
        Ok(mut cfg) if cfg.schema_version <= CONFIG_SCHEMA_VERSION => {
            if cfg.schema_version < CONFIG_SCHEMA_VERSION {
                // No structural migrations exist between 0 and 1 — 0 only
                // means "written before the version field existed". Stamp it
                // so the next save records the schema the file conforms to.
                cfg.schema_version = CONFIG_SCHEMA_VERSION;
            }
            Ok(cfg)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config_json() -> serde_json::Value {
        serde_json::to_value(Configuration::new()).unwrap()
    }

    /// The quarantine path inside parse_or_quarantine renames a real file;
    /// pointing it at a path that never exists keeps these tests pure (the
    /// rename fails, which quarantine() logs and tolerates by design).
    const NO_FILE: &str = "/nonexistent/razercontrol-test/daemon.json";

    #[test]
    fn schema_zero_is_stamped_to_current_on_load() {
        let mut v = valid_config_json();
        v["schema_version"] = 0.into();
        let cfg = parse_or_quarantine(NO_FILE, &v.to_string()).unwrap();
        assert_eq!(cfg.schema_version, CONFIG_SCHEMA_VERSION);
    }

    #[test]
    fn missing_schema_field_loads_as_zero_and_is_stamped() {
        let mut v = valid_config_json();
        v.as_object_mut().unwrap().remove("schema_version");
        let cfg = parse_or_quarantine(NO_FILE, &v.to_string()).unwrap();
        assert_eq!(cfg.schema_version, CONFIG_SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_is_refused_not_reinterpreted() {
        let mut v = valid_config_json();
        v["schema_version"] = (CONFIG_SCHEMA_VERSION + 1).into();
        let err = parse_or_quarantine(NO_FILE, &v.to_string())
            .err()
            .expect("a newer schema must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn garbage_contents_are_refused_as_invalid_data() {
        let err = parse_or_quarantine(NO_FILE, "definitely not json")
            .err()
            .expect("garbage must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}

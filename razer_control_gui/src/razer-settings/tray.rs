use std::fs;
use std::sync::{Arc, Mutex};

// Reduced to the battery/charge + fan surface: per-component sensor
// monitoring (CPU/iGPU/dGPU temps, powers, utilisation) was removed — the
// dGPU variants forced the discrete GPU awake on every poll, and the rest
// is deliberately out of scope for this tool.
#[derive(Default, Clone)]
pub struct SensorState {
    pub fan_speed: Option<i32>,
    pub on_ac: Option<bool>,
    pub battery_pct: Option<u8>,
    pub battery_status: Option<String>,
    pub battery_power: Option<f64>,
}

impl SensorState {
    /// Read battery/AC state directly from sysfs (fan requires the daemon)
    fn read_fresh() -> Self {
        SensorState {
            fan_speed: None, // requires daemon, skip in tray
            on_ac: read_ac_power(),
            battery_pct: read_battery_pct(),
            battery_status: read_battery_status(),
            battery_power: read_battery_power(),
        }
    }

    fn has_data(&self) -> bool {
        self.fan_speed.is_some() || self.on_ac.is_some()
    }

    fn format_lines(&self) -> String {
        let mut lines: Vec<String> = Vec::new();




        if let Some(rpm) = self.fan_speed {
            if rpm == 0 {
                lines.push("Fan: Auto".into());
            } else {
                lines.push(format!("Fan: {} RPM", rpm));
            }
        }

        match (self.on_ac, self.battery_pct) {
            (Some(true), Some(pct)) => {
                let mut text = format!("AC / {}%", pct);
                if let Some(ref status) = self.battery_status {
                    if let Some(w) = self.battery_power {
                        if status == "Charging" {
                            text = format!("AC / {}% +{:.1}W", pct, w);
                        }
                    }
                    if status == "Not charging" {
                        text = format!("AC / {}% (Limit)", pct);
                    }
                }
                lines.push(text);
            }
            (Some(true), None) => lines.push("AC Power".into()),
            (Some(false), Some(pct)) => {
                let mut text = format!("Battery {}%", pct);
                if let Some(w) = self.battery_power {
                    text = format!("Battery {}% \u{2212}{:.1}W", pct, w);
                }
                lines.push(text);
            }
            (Some(false), None) => lines.push("Battery".into()),
            _ => {}
        }

        if lines.is_empty() {
            "Razer Control".into()
        } else {
            lines.join("\n")
        }
    }
}

pub type SharedSensorState = Arc<Mutex<SensorState>>;

pub fn new_shared_state() -> SharedSensorState {
    Arc::new(Mutex::new(SensorState::default()))
}

pub struct RazerTray {
    state: SharedSensorState,
}

impl RazerTray {
    pub fn new(state: SharedSensorState) -> Self {
        RazerTray { state }
    }
}

impl ksni::Tray for RazerTray {
    fn id(&self) -> String {
        "razer-settings".into()
    }

    fn title(&self) -> String {
        "Razer Control".into()
    }

    fn icon_name(&self) -> String {
        "com.github.encomjp.razercontrol".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        // Try shared state first (has fan speed from daemon); fall back to direct reads
        let body = if let Ok(s) = self.state.lock() {
            if s.has_data() {
                s.format_lines()
            } else {
                drop(s);
                SensorState::read_fresh().format_lines()
            }
        } else {
            SensorState::read_fresh().format_lines()
        };

        ksni::ToolTip {
            title: "Razer Control".into(),
            description: body,
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label: "Open Razer Control".into(),
                activate: Box::new(|_| {
                    // Use GApplication activation via command line — this sends
                    // an "activate" signal to the already-running primary instance
                    // rather than spawning a duplicate process with a second tray.
                    let _ = std::process::Command::new("gdbus")
                        .args([
                            "call", "--session",
                            "--dest", "com.encomjp.razer-settings",
                            "--object-path", "/com/encomjp/razer_settings",
                            "--method", "org.gtk.Application.Activate",
                            "[]",
                        ])
                        .spawn();
                }),
                ..Default::default()
            }),
            ksni::MenuItem::Separator,
            ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_| {
                    std::process::exit(0);
                }),
                ..Default::default()
            }),
        ]
    }
}

// --- Sensor reading functions (standalone, no daemon dependency) ---




fn read_ac_power() -> Option<bool> {
    for name in ["AC0", "ADP0", "ADP1", "ACAD"] {
        let path = format!("/sys/class/power_supply/{}/online", name);
        if let Ok(content) = fs::read_to_string(&path) {
            return Some(content.trim() == "1");
        }
    }
    None
}

fn read_battery_pct() -> Option<u8> {
    for bat in ["BAT0", "BAT1"] {
        let path = format!("/sys/class/power_supply/{}/capacity", bat);
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(pct) = content.trim().parse::<u8>() {
                return Some(pct);
            }
        }
    }
    None
}






fn read_battery_status() -> Option<String> {
    for bat in ["BAT0", "BAT1"] {
        let path = format!("/sys/class/power_supply/{}/status", bat);
        if let Ok(content) = fs::read_to_string(&path) {
            let s = content.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn read_battery_power() -> Option<f64> {
    for bat in ["BAT0", "BAT1"] {
        let c_path = format!("/sys/class/power_supply/{}/current_now", bat);
        let v_path = format!("/sys/class/power_supply/{}/voltage_now", bat);
        if let (Ok(c_str), Ok(v_str)) = (fs::read_to_string(&c_path), fs::read_to_string(&v_path)) {
            if let (Ok(c), Ok(v)) = (c_str.trim().parse::<u64>(), v_str.trim().parse::<u64>()) {
                if c > 0 {
                    return Some(c as f64 * v as f64 / 1e12);
                }
            }
        }
    }
    None
}


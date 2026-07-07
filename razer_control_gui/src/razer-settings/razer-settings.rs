use gtk4 as gtk;
use libadwaita as adw;
use gtk::prelude::*;
use adw::prelude::*;
use std::fs;
use std::rc::Rc;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Duration;

#[path = "../comms.rs"]
mod comms;
mod error_handling;
mod widgets;
mod util;
mod tray;

use service::SupportedDevice;
use error_handling::*;
use widgets::*;
use util::*;

/// Commit a slider's value to the daemon when the user finishes adjusting it,
/// not on every intermediate change. A pointer drag emits a stream of
/// `value_changed` ticks, and each `action` is a blocking IPC round-trip on the
/// GTK main thread; firing per tick freezes the UI mid-drag. We track the
/// pointer button with a raw event controller, set `dragging` while it is held,
/// and commit the final value only on release. Keyboard and scroll-wheel changes are
/// discrete (not a drag), so they fall through `value_changed` and commit
/// immediately. `dragging` is owned by the caller so its periodic refresh can
/// skip overwriting the slider while the user is mid-drag; `refreshing`
/// suppresses programmatic updates.
fn connect_commit_on_release<F: Fn(f64) + 'static>(
    scale: &gtk::Scale,
    refreshing: Rc<Cell<bool>>,
    dragging: Rc<Cell<bool>>,
    action: F,
) {
    let action = Rc::new(action);

    // Observe the raw pointer button with EventControllerLegacy, not a
    // GestureClick: a click gesture cancels the moment a press turns into a drag,
    // which would clear `dragging` mid-drag and drop us back to a blocking send
    // per tick. A legacy controller sees button press/release unconditionally and
    // never claims the event, so the Scale still drags normally.
    let controller = gtk::EventControllerLegacy::new();
    {
        let dragging = dragging.clone();
        let refreshing = refreshing.clone();
        let action = action.clone();
        let scale = scale.clone();
        controller.connect_event(move |_, event| {
            match event.event_type() {
                gtk::gdk::EventType::ButtonPress => dragging.set(true),
                gtk::gdk::EventType::ButtonRelease => {
                    dragging.set(false);
                    if !refreshing.get() {
                        action(scale.value());
                    }
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
    }
    scale.add_controller(controller);

    scale.connect_value_changed(move |sc| {
        // Ignore programmatic refreshes and the mid-drag stream; the release
        // handler commits the final dragged value. Keyboard/scroll changes pass
        // through and commit immediately.
        if refreshing.get() || dragging.get() {
            return;
        }
        action(sc.value());
    });
}

fn send_data(opt: comms::DaemonCommand) -> Option<comms::DaemonResponse> {
    match comms::try_bind() {
        Ok(socket) => comms::send_to_daemon(opt, socket),
        Err(error) => {
            eprintln!("Can't connect to daemon: {}", error);
            None
        }
    }
}


fn get_device_name() -> Option<String> {
    let response = send_data(comms::DaemonCommand::GetDeviceName)?;
    use comms::DaemonResponse::*;
    match response {
        GetDeviceName { name } => Some(name),
        response => {
            println!("Instead of GetDeviceName got {response:?}");
            None
        }
    }
}

/// Show an error dialog to the user without panicking.
/// This is safe to call from GTK signal callbacks (no panic/unwind).
fn show_error_dialog(app: &adw::Application, message: &str) {
    let dialog = adw::MessageDialog::new(
        app.active_window().as_ref(),
        Some("Razer Control — Error"),
        Some(message),
    );
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.connect_response(None, |dlg, _| {
        dlg.close();
    });
    dialog.present();
}

fn get_bho() -> Option<(bool, u8)> {
    let response = send_data(comms::DaemonCommand::GetBatteryHealthOptimizer())?;
    use comms::DaemonResponse::*;
    match response {
        GetBatteryHealthOptimizer { is_on, threshold } => Some((is_on, threshold)),
        response => {
            println!("Instead of GetBatteryHealthOptimizer got {response:?}");
            None
        }
    }
}

fn set_bho(is_on: bool, threshold: u8) -> Option<bool> {
    let response = send_data(comms::DaemonCommand::SetBatteryHealthOptimizer { is_on, threshold })?;
    use comms::DaemonResponse::*;
    match response {
        SetBatteryHealthOptimizer { result } => Some(result),
        response => {
            println!("Instead of SetBatteryHealthOptimizer got {response:?}");
            None
        }
    }
}

fn get_brightness(ac: bool) -> Option<u8> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::GetBrightness{ ac })?;
    use comms::DaemonResponse::*;
    match response {
        GetBrightness { result } => Some(result),
        response => {
            println!("Instead of GetBrightness got {response:?}");
            None
        }
    }
}

fn set_brightness(ac: bool, val: u8) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::SetBrightness { ac, val })?;
    use comms::DaemonResponse::*;
    match response {
        SetBrightness { result } => Some(result),
        response => {
            println!("Instead of SetBrightness got {response:?}");
            None
        }
    }
}

fn get_logo(ac: bool) -> Option<u8> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::GetLogoLedState{ ac })?;
    use comms::DaemonResponse::*;
    match response {
        GetLogoLedState { logo_state } => Some(logo_state),
        response => {
            println!("Instead of GetLogoLedState got {response:?}");
            None
        }
    }
}

fn set_logo(ac: bool, logo_state: u8) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::SetLogoLedState{ ac , logo_state })?;
    use comms::DaemonResponse::*;
    match response {
        SetLogoLedState { result } => Some(result),
        response => {
            println!("Instead of SetLogoLedState got {response:?}");
            None
        }
    }
}

fn get_standard_effect() -> Option<(u8, Vec<u8>)> {
    let response = send_data(comms::DaemonCommand::GetStandardEffect)?;
    use comms::DaemonResponse::*;
    match response {
        GetStandardEffect { effect, params } => Some((effect, params)),
        response => {
            println!("Instead of GetStandardEffect got {response:?}");
            None
        }
    }
}

fn set_effect(name: &str, values: Vec<u8>) -> Option<bool> {
    let response = send_data(comms::DaemonCommand::SetEffect { name: name.into(), params: values })?;
    use comms::DaemonResponse::*;
    match response {
        SetEffect { result } => Some(result),
        response => {
            println!("Instead of SetEffect got {response:?}");
            None
        }
    }
}

fn get_power(ac: bool) -> Option<(u8, u8, u8)> {
    let ac = if ac { 1 } else { 0 };
    let mut result = (0, 0, 0);

    let response = send_data(comms::DaemonCommand::GetPwrLevel{ ac })?;
    use comms::DaemonResponse::*;
    match response {
        GetPwrLevel { pwr } => result.0 = pwr,
        response => {
            println!("Instead of GetPwrLevel got {response:?}");
            return None
        }
    }

    let response = send_data(comms::DaemonCommand::GetCPUBoost { ac })?;
    match response {
        GetCPUBoost { cpu } => result.1 = cpu,
        response => {
            println!("Instead of GetCPUBoost got {response:?}");
            return None
        }
    }

    let response = send_data(comms::DaemonCommand::GetGPUBoost { ac })?;
    match response {
        GetGPUBoost { gpu } => result.2 = gpu,
        response => {
            println!("Instead of GetGPUBoost got {response:?}");
            return None
        }
    }
    Some(result)
}

fn set_power(ac: bool, power: (u8, u8, u8)) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::SetPowerMode { ac, pwr: power.0, cpu: power.1, gpu: power.2 })?;
    use comms::DaemonResponse::*;
    match response {
        SetPowerMode { result } => Some(result),
        response => {
            println!("Instead of SetPowerMode got {response:?}");
            None
        }
    }
}

/// One-shot GPU inventory (lspci/sysfs via the daemon; never wakes the dGPU).
/// v2.8 slim: the response carries only the list + PM policy — the envycontrol
/// fields and the suspend toggle went with the scope cut. Sole GUI use is the
/// monitor's one-time iGPU/dGPU row labelling at construction.
fn get_gpu_status() -> Option<Vec<comms::GpuInfo>> {
    let response = send_data(comms::DaemonCommand::GetGpuStatus)?;
    use comms::DaemonResponse::*;
    match response {
        GetGpuStatus { gpus, .. } => Some(gpus),
        response => {
            println!("Instead of GetGpuStatus got {response:?}");
            None
        }
    }
}

fn get_fan_speed(ac: bool) -> Option<i32> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::GetFanSpeed{ ac })?;
    use comms::DaemonResponse::*;
    match response {
        GetFanSpeed { rpm } => Some(rpm),
        response => {
            println!("Instead of GetFanSpeed got {response:?}");
            None
        }
    }
}

fn set_fan_speed(ac: bool, value: i32) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::SetFanSpeed{ ac, rpm: value })?;
    use comms::DaemonResponse::*;
    match response {
        SetFanSpeed { result } => Some(result),
        response => {
            println!("Instead of SetFanSpeed got {response:?}");
            None
        }
    }
}

fn get_fan_curve(ac: bool) -> Option<comms::FanCurve> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::GetFanCurve { ac })?;
    use comms::DaemonResponse::*;
    match response {
        GetFanCurve { curve } => Some(curve),
        response => {
            println!("Instead of GetFanCurve got {response:?}");
            None
        }
    }
}

fn set_fan_curve(ac: bool, curve: comms::FanCurve) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    let response = send_data(comms::DaemonCommand::SetFanCurve { ac, curve })?;
    use comms::DaemonResponse::*;
    match response {
        SetFanCurve { result } => Some(result),
        response => {
            println!("Instead of SetFanCurve got {response:?}");
            None
        }
    }
}











/// Read CPU temperature from hwmon (supports AMD k10temp/zenpower and Intel coretemp)
fn get_cpu_temperature() -> Option<f64> {
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let name_path = entry.path().join("name");
            if let Ok(name) = fs::read_to_string(&name_path) {
                let name = name.trim();
                if name == "k10temp" || name == "zenpower" || name == "coretemp" {
                    let temp_path = entry.path().join("temp1_input");
                    if let Ok(content) = fs::read_to_string(&temp_path) {
                        if let Ok(temp) = content.trim().parse::<f64>() {
                            return Some(temp / 1000.0);
                        }
                    }
                }
            }
        }
    }

    let paths = [
        "/sys/class/thermal/thermal_zone0/temp",
        "/sys/class/thermal/thermal_zone1/temp",
        "/sys/class/thermal/thermal_zone2/temp",
    ];

    for path in paths {
        if let Ok(content) = fs::read_to_string(path) {
            if let Ok(temp) = content.trim().parse::<f64>() {
                return Some(temp / 1000.0);
            }
        }
    }
    None
}

/// Read CPU utilization from /proc/stat (delta-based)
fn get_cpu_utilization() -> Option<u32> {
    static LAST_IDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static LAST_TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    if let Ok(content) = fs::read_to_string("/proc/stat") {
        if let Some(line) = content.lines().next() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 5 && fields[0] == "cpu" {
                let mut total: u64 = 0;
                for f in &fields[1..] {
                    if let Ok(v) = f.parse::<u64>() {
                        total += v;
                    }
                }
                let idle = fields[4].parse::<u64>().unwrap_or(0);

                let prev_idle = LAST_IDLE.swap(idle, std::sync::atomic::Ordering::Relaxed);
                let prev_total = LAST_TOTAL.swap(total, std::sync::atomic::Ordering::Relaxed);

                if prev_total > 0 {
                    let d_idle = idle.wrapping_sub(prev_idle);
                    let d_total = total.wrapping_sub(prev_total);
                    if d_total > 0 {
                        let usage = 100.0 * (1.0 - d_idle as f64 / d_total as f64);
                        return Some(usage.round() as u32);
                    }
                }
            }
        }
    }
    None
}

/// Read system/CPU power consumption from RAPL (supports AMD and Intel)
fn get_system_power() -> Option<f64> {
    let energy_paths = [
        "/sys/class/powercap/amd-rapl:0/energy_uj",
        "/sys/class/powercap/amd_rapl/amd-rapl:0/energy_uj",
        "/sys/class/powercap/intel-rapl:0/energy_uj",
        "/sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj",
    ];

    for path in &energy_paths {
        if let Ok(content) = fs::read_to_string(path) {
            if let Ok(energy) = content.trim().parse::<u64>() {
                static LAST_ENERGY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                static LAST_TIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as u64;

                let prev_energy = LAST_ENERGY.swap(energy, std::sync::atomic::Ordering::Relaxed);
                let prev_time = LAST_TIME.swap(now, std::sync::atomic::Ordering::Relaxed);

                if prev_energy > 0 && prev_time > 0 && energy > prev_energy {
                    let delta_energy = energy - prev_energy;
                    let delta_time = now - prev_time;
                    if delta_time > 0 {
                        let power = delta_energy as f64 / delta_time as f64;
                        return Some(power);
                    }
                }
                return None; // Found path but need second reading
            }
        }
    }
    None
}

/// Read iGPU temperature from amdgpu hwmon
fn get_igpu_temperature() -> Option<f64> {
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let name_path = entry.path().join("name");
            if let Ok(name) = fs::read_to_string(&name_path) {
                if name.trim() == "amdgpu" {
                    for temp_file in ["temp1_input", "temp2_input"] {
                        let temp_path = entry.path().join(temp_file);
                        if let Ok(content) = fs::read_to_string(&temp_path) {
                            if let Ok(temp) = content.trim().parse::<f64>() {
                                return Some(temp / 1000.0);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Read iGPU power from hwmon (AMD amdgpu) or Intel RAPL GT fallback
fn get_igpu_power() -> Option<f64> {
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let name_path = entry.path().join("name");
            if let Ok(name) = fs::read_to_string(&name_path) {
                if name.trim() == "amdgpu" {
                    let power_path = entry.path().join("power1_average");
                    if let Ok(content) = fs::read_to_string(&power_path) {
                        if let Ok(power_uw) = content.trim().parse::<f64>() {
                            return Some(power_uw / 1_000_000.0);
                        }
                    }
                }
            }
        }
    }

    // Fallback: Intel RAPL GT domain
    let paths = [
        "/sys/class/powercap/intel-rapl:0:1/energy_uj",
        "/sys/class/powercap/intel-rapl/intel-rapl:0/intel-rapl:0:1/energy_uj",
    ];

    for path in &paths {
        if let Ok(content) = fs::read_to_string(path) {
            if let Ok(energy) = content.trim().parse::<u64>() {
                static LAST_IGPU_ENERGY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                static LAST_IGPU_TIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as u64;

                let prev_energy = LAST_IGPU_ENERGY.swap(energy, std::sync::atomic::Ordering::Relaxed);
                let prev_time = LAST_IGPU_TIME.swap(now, std::sync::atomic::Ordering::Relaxed);

                if prev_energy > 0 && prev_time > 0 && energy > prev_energy {
                    let delta_energy = energy - prev_energy;
                    let delta_time = now - prev_time;
                    if delta_time > 0 {
                        return Some(delta_energy as f64 / delta_time as f64);
                    }
                }
            }
        }
    }
    None
}

/// Read iGPU utilization (AMD gpu_busy_percent or Intel freq-based fallback)
fn get_igpu_utilization() -> Option<u32> {
    for card in ["card0", "card1", "card2"] {
        let busy_path = format!("/sys/class/drm/{}/device/gpu_busy_percent", card);
        if let Ok(content) = fs::read_to_string(&busy_path) {
            if let Ok(util) = content.trim().parse::<u32>() {
                let driver_path = format!("/sys/class/drm/{}/device/driver", card);
                if let Ok(driver_link) = fs::read_link(&driver_path) {
                    if driver_link.to_string_lossy().contains("amdgpu") {
                        return Some(util);
                    }
                }
            }
        }
    }

    // Fallback: frequency-based estimation for Intel
    let paths = [
        "/sys/class/drm/card0/gt/gt0/rps_act_freq_mhz",
        "/sys/class/drm/card1/gt/gt0/rps_act_freq_mhz",
    ];
    let max_paths = [
        "/sys/class/drm/card0/gt/gt0/rps_max_freq_mhz",
        "/sys/class/drm/card1/gt/gt0/rps_max_freq_mhz",
    ];

    for (i, path) in paths.iter().enumerate() {
        if let Ok(act_content) = fs::read_to_string(path) {
            if let Ok(max_content) = fs::read_to_string(&max_paths[i]) {
                if let (Ok(act), Ok(max)) = (
                    act_content.trim().parse::<f64>(),
                    max_content.trim().parse::<f64>()
                ) {
                    if max > 0.0 {
                        return Some(((act / max) * 100.0) as u32);
                    }
                }
            }
        }
    }
    None
}

/// Read dGPU temperature (NVIDIA)
/// dGPU sensors come exclusively from the daemon's fan-curve cache
/// (GetDgpuSensors). The GUI never runs nvidia-smi itself: when no smart curve
/// with a GPU/Both source is actively sampling, this returns None and the dGPU
/// row simply stays hidden — by design, so a (possibly tray-hidden) GUI can
/// never generate GPU/driver traffic during gameplay. The daemon additionally
/// age-gates the cache, so no stale values from a finished session leak here.
fn get_dgpu_sensors() -> Option<comms::DgpuSensors> {
    match send_data(comms::DaemonCommand::GetDgpuSensors) {
        Some(comms::DaemonResponse::GetDgpuSensors { sensors }) => sensors,
        _ => None,
    }
}

/// Read NVIDIA dGPU power consumption
/// Read battery percentage from /sys/class/power_supply/BAT{0,1}/capacity
fn get_battery_percentage() -> Option<u8> {
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

/// Read battery status (Charging, Discharging, Full, Not charging)
fn get_battery_status() -> Option<String> {
    for bat in ["BAT0", "BAT1"] {
        let path = format!("/sys/class/power_supply/{}/status", bat);
        if let Ok(content) = fs::read_to_string(&path) {
            let status = content.trim().to_string();
            if !status.is_empty() {
                return Some(status);
            }
        }
    }
    None
}

/// Read battery power draw in watts (current_now * voltage_now)
fn get_battery_power() -> Option<f64> {
    for bat in ["BAT0", "BAT1"] {
        let current_path = format!("/sys/class/power_supply/{}/current_now", bat);
        let voltage_path = format!("/sys/class/power_supply/{}/voltage_now", bat);
        if let (Ok(c_str), Ok(v_str)) = (fs::read_to_string(&current_path), fs::read_to_string(&voltage_path)) {
            if let (Ok(current_ua), Ok(voltage_uv)) = (c_str.trim().parse::<u64>(), v_str.trim().parse::<u64>()) {
                if current_ua > 0 {
                    return Some(current_ua as f64 * voltage_uv as f64 / 1e12);
                }
            }
        }
    }
    None
}


/// Create system monitor panel at the bottom (widget-style layout)
fn create_system_monitor(shared_state: tray::SharedSensorState) -> gtk::Box {
    // Two-state monitor, keyed on the fan mode of the ACTIVE power domain:
    //  * Fan mode "Smart Curve" (curve.enabled): the classic full panel —
    //    CPU / iGPU / dGPU rows with temperature, power and utilisation, plus
    //    the toolbar backdrop. In this state the dGPU is being temperature-
    //    managed anyway, so reading its sensors is fair game — but reads are
    //    still gated on the dGPU being runtime-ACTIVE (sysfs check via the
    //    daemon), so a sleeping dGPU is never woken just to be displayed.
    //  * Any other fan mode: battery/charge + fan line only. No per-component
    //    sensor reads, no nvidia-smi, no toolbar backdrop (its background top
    //    edge otherwise renders as a stray hairline above the battery line).
    let main_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main_box.set_margin_start(16);
    main_box.set_margin_end(16);
    main_box.set_margin_top(4);
    main_box.set_margin_bottom(4);

    // Helper: create a full-width monitor row (name + temp on left, power · util% on right)
    fn make_row(label_text: &str) -> (gtk::Box, gtk::Label, gtk::Label, gtk::Label, gtk::Label) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_top(1);
        row.set_margin_bottom(1);

        let name = gtk::Label::new(Some(label_text));
        name.add_css_class("caption");
        name.set_xalign(0.0);
        name.set_opacity(0.6);
        row.append(&name);

        let temp = gtk::Label::new(None);
        temp.add_css_class("caption");
        temp.add_css_class("numeric");
        row.append(&temp);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        row.append(&spacer);

        let power = gtk::Label::new(None);
        power.add_css_class("caption");
        power.add_css_class("numeric");
        power.set_opacity(0.7);
        row.append(&power);

        let dot = gtk::Label::new(Some("\u{00B7}"));
        dot.add_css_class("caption");
        dot.set_opacity(0.3);
        row.append(&dot);

        let util = gtk::Label::new(None);
        util.add_css_class("caption");
        util.add_css_class("numeric");
        util.set_opacity(0.7);
        row.append(&util);

        (row, temp, power, dot, util)
    }

    let cpu_name = util::get_cpu_name().unwrap_or_else(|| "CPU".to_string());
    // Shorten extremely long CPU names (e.g. "AMD Ryzen 9 7945HX with Radeon Graphics" -> "AMD Ryzen 9 7945HX")
    let cpu_label = cpu_name.replace(" with Radeon Graphics", "").replace(" 16-Core Processor", "");

    // Fetch detected GPUs to find names
    let mut igpu_label = "iGPU".to_string();
    let mut dgpu_label = "dGPU".to_string();
    
    if let Some(gpu_list) = get_gpu_status() {
        for gpu_info in gpu_list {
           let name = gpu_info.name;
           // Heuristic: NVIDIA/Discrete usually dGPU; AMD/Intel usually iGPU (unless discrete)
           if name.to_uppercase().contains("NVIDIA") {
               dgpu_label = name.replace(" Laptop GPU", "");
           } else if name.to_uppercase().contains("AMD") || name.to_uppercase().contains("INTEL") {
               // Assume the first non-NVIDIA is iGPU
               if igpu_label == "iGPU" {
                   igpu_label = name.replace(" Radeon Graphics", "");
               }
           }
        }
    }

    let (cpu_row, cpu_temp_l, cpu_power_l, cpu_dot, cpu_util_l) = make_row(&cpu_label);
    let (igpu_row, igpu_temp_l, igpu_power_l, igpu_dot, igpu_util_l) = make_row(&igpu_label);
    let (dgpu_row, dgpu_temp_l, dgpu_power_l, dgpu_dot, dgpu_util_l) = make_row(&dgpu_label);

    // Battery + Fan row (status + watts on left, fan on right)
    let bottom_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bottom_row.set_margin_top(1);
    bottom_row.set_margin_bottom(1);

    let bat_status_l = gtk::Label::new(None);
    bat_status_l.add_css_class("caption");
    bat_status_l.set_xalign(0.0);
    bat_status_l.set_opacity(0.6);
    let bat_pct_l = gtk::Label::new(None);
    bat_pct_l.add_css_class("caption");
    bat_pct_l.add_css_class("numeric");
    let bat_watts_l = gtk::Label::new(None);
    bat_watts_l.add_css_class("caption");
    bat_watts_l.add_css_class("numeric");
    bat_watts_l.set_opacity(0.7);

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);

    let fan_l = gtk::Label::new(None);
    fan_l.add_css_class("caption");
    fan_l.add_css_class("numeric");
    fan_l.set_opacity(0.6);

    bottom_row.append(&bat_status_l);
    bottom_row.append(&bat_pct_l);
    bottom_row.append(&bat_watts_l);
    bottom_row.append(&spacer);
    bottom_row.append(&fan_l);

    main_box.append(&cpu_row);
    main_box.append(&igpu_row);
    main_box.append(&dgpu_row);
    main_box.append(&bottom_row);

    // Metric rows start hidden; the poll below shows them in smart-curve mode.
    cpu_row.set_visible(false);
    igpu_row.set_visible(false);
    dgpu_row.set_visible(false);

    // GTK objects are reference-counted: clone a handle for the closure so the
    // original binding can still be returned below.
    let monitor_box = main_box.clone();
    glib::timeout_add_local(Duration::from_secs(2), move || {
        // Close-to-tray keeps this timer alive; while the window is hidden the
        // widget is unmapped — skip the whole tick (daemon socket calls, the
        // EC fan-setting read, sysfs sweeps) instead of polling invisibly all
        // day (measured: ~27k daemon requests in half a day from the two GUI
        // timers). Tradeoff: the tray tooltip freezes at the last visible
        // values while the window is hidden.
        if !monitor_box.is_mapped() {
            return glib::ControlFlow::Continue;
        }
        let on_ac = check_if_running_on_ac_power();
        let ac = on_ac.unwrap_or(true);
        let fan = get_fan_speed(ac);
        let battery_pct = get_battery_percentage();
        let battery_status = get_battery_status();
        let battery_power = get_battery_power();

        // Gate: full metrics only while the active domain's fan mode is the
        // smart temperature curve.
        let smart = get_fan_curve(ac).map(|c| c.enabled).unwrap_or(false);

        if smart {
            monitor_box.add_css_class("toolbar");
            monitor_box.add_css_class("monitor-bar");

            let cpu_temp = get_cpu_temperature();
            let igpu_temp = get_igpu_temperature();
            let sys_power = get_system_power();
            let cpu_util = get_cpu_utilization();
            let igpu_pwr = get_igpu_power();
            let igpu_util = get_igpu_utilization();

            // dGPU sensors come solely from the daemon's curve-task cache: no
            // nvidia-smi from the GUI, and no extra GetGpuStatus/PCI scan per
            // tick (freshness of the cache already implies the dGPU is awake).
            // None — no GPU/Both curve sampling, dGPU asleep, or the entry
            // aged out — simply hides the row.
            let (dgpu_temp, dgpu_pwr, dgpu_util) = match get_dgpu_sensors() {
                Some(s) => (Some(s.temp_c), s.power_w, s.util_pct),
                None => (None, None, None),
            };

            // CPU
            match cpu_temp {
                Some(t) => { cpu_temp_l.set_text(&format!("{:.0}\u{00B0}C", t)); cpu_row.set_visible(true); }
                None => cpu_row.set_visible(false),
            }
            match sys_power {
                Some(w) => { cpu_power_l.set_text(&format!("{:.1} W", w)); cpu_power_l.set_visible(true); }
                None => cpu_power_l.set_visible(false),
            }
            match cpu_util {
                Some(u) => { cpu_util_l.set_text(&format!("{}%", u)); cpu_util_l.set_visible(true); cpu_dot.set_visible(sys_power.is_some()); }
                None => { cpu_util_l.set_visible(false); cpu_dot.set_visible(false); }
            }

            // iGPU
            let igpu_has = igpu_temp.is_some() || igpu_pwr.is_some();
            igpu_row.set_visible(igpu_has);
            if igpu_has {
                match igpu_temp {
                    Some(t) => { igpu_temp_l.set_text(&format!("{:.0}\u{00B0}C", t)); igpu_temp_l.set_visible(true); }
                    None => igpu_temp_l.set_visible(false),
                }
                match igpu_pwr {
                    Some(w) => { igpu_power_l.set_text(&format!("{:.1} W", w)); igpu_power_l.set_visible(true); }
                    None => igpu_power_l.set_visible(false),
                }
                match igpu_util {
                    Some(u) => { igpu_util_l.set_text(&format!("{}%", u)); igpu_util_l.set_visible(true); igpu_dot.set_visible(igpu_pwr.is_some()); }
                    None => { igpu_util_l.set_visible(false); igpu_dot.set_visible(false); }
                }
            }

            // dGPU
            match dgpu_temp {
                Some(t) => { dgpu_temp_l.set_text(&format!("{:.0}\u{00B0}C", t)); dgpu_row.set_visible(true); }
                None => dgpu_row.set_visible(false),
            }
            match dgpu_pwr {
                Some(w) => { dgpu_power_l.set_text(&format!("{:.1} W", w)); dgpu_power_l.set_visible(true); }
                None => dgpu_power_l.set_visible(false),
            }
            match dgpu_util {
                Some(u) => { dgpu_util_l.set_text(&format!("{}%", u)); dgpu_util_l.set_visible(true); dgpu_dot.set_visible(dgpu_pwr.is_some()); }
                None => { dgpu_util_l.set_visible(false); dgpu_dot.set_visible(false); }
            }
        } else {
            monitor_box.remove_css_class("toolbar");
            monitor_box.remove_css_class("monitor-bar");
            cpu_row.set_visible(false);
            igpu_row.set_visible(false);
            dgpu_row.set_visible(false);
        }

        // Battery + Fan bottom row (always)
        match battery_pct {
            Some(pct) => {
                let status_text = match battery_status.as_deref() {
                    Some("Charging") => "Charging",
                    Some("Not charging") => "Full (Limit)",
                    Some("Full") => "Full",
                    Some("Discharging") => "Battery",
                    _ => "Battery",
                };
                bat_status_l.set_text(status_text);
                bat_pct_l.set_text(&format!("{}%", pct));
                bat_status_l.set_visible(true);
                bat_pct_l.set_visible(true);
                match (battery_status.as_deref(), battery_power) {
                    (Some("Charging"), Some(w)) => {
                        bat_watts_l.set_text(&format!("+{:.1}W", w));
                        bat_watts_l.set_visible(true);
                    }
                    (Some("Discharging"), Some(w)) => {
                        bat_watts_l.set_text(&format!("\u{2212}{:.1}W", w));
                        bat_watts_l.set_visible(true);
                    }
                    _ => bat_watts_l.set_visible(false),
                }
            }
            None => {
                bat_status_l.set_visible(false);
                bat_pct_l.set_visible(false);
                bat_watts_l.set_visible(false);
            }
        }

        match fan {
            Some(0) => { fan_l.set_text("Fan: Auto"); fan_l.set_visible(true); }
            Some(rpm) => { fan_l.set_text(&format!("Fan: {} RPM", rpm)); fan_l.set_visible(true); }
            None => fan_l.set_visible(false),
        }

        // Write snapshot to shared state for tray tooltip (battery/fan surface)
        if let Ok(mut state) = shared_state.lock() {
            state.fan_speed = fan;
            state.on_ac = on_ac;
            state.battery_pct = battery_pct;
            state.battery_status = battery_status.clone();
            state.battery_power = battery_power;
        }

        glib::ControlFlow::Continue
    });

    main_box
}




fn main() {
    setup_panic_hook();

    // Shared sensor state for tray tooltip
    let shared_state = tray::new_shared_state();

    let app = adw::Application::builder()
        .application_id("com.encomjp.razer-settings")
        .flags(gtk::gio::ApplicationFlags::empty())
        .build();

    // Keep the app alive even when the window is hidden (close-to-tray)
    let _hold_guard = app.hold();

    // Spawn tray only on primary instance (inside connect_startup)
    let tray_state = Arc::clone(&shared_state);
    app.connect_startup(move |_| {
        adw::init().ok();

        let style_manager = adw::StyleManager::default();
        style_manager.set_color_scheme(adw::ColorScheme::ForceDark);

        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("../style.css"));
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().expect("Could not connect to a display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        // Spawn KDE system tray icon (only on primary instance)
        let tray = tray::RazerTray::new(Arc::clone(&tray_state));
        {
            use ksni::blocking::TrayMethods;
            match tray.spawn() {
                Ok(_handle) => {} // tray runs in background thread
                Err(e) => eprintln!("Tray error (non-fatal): {}", e),
            }
        }
    });

    let shared_state_for_activate = Arc::clone(&shared_state);
    app.connect_activate(move |app| {
        // If a window already exists (even if hidden), show it
        let windows = app.windows();
        if !windows.is_empty() {
            let win = &windows[0];
            win.set_visible(true);
            win.present();
            return;
        }

        let device_file = std::fs::read_to_string(service::device_file_path()).unwrap_or("[]".into());
        let devices: Vec<SupportedDevice> = match serde_json::from_str(&device_file) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to parse device file: {}", e);
                show_error_dialog(app, &format!(
                    "Failed to parse device file ({}).\n\nPlease ensure razercontrol is installed correctly.",
                    e
                ));
                return;
            }
        };

        let device_name = match get_device_name() {
            Some(name) => name,
            None => {
                eprintln!("Failed to get device name from daemon");
                show_error_dialog(app, 
                    "Failed to get device name.\n\n\
                    The daemon may not be running or failed to respond.\n\
                    Try: systemctl --user restart razercontrol"
                );
                return;
            }
        };

        let device = match devices.iter().find(|d| d.name == device_name) {
            Some(d) => d.clone(),
            None => {
                eprintln!("Device '{}' not found in laptops.json", device_name);
                show_error_dialog(app, &format!(
                    "Device '{}' not found in supported devices list.\n\n\
                    Your device may not be supported yet, or the device database is outdated.",
                    device_name
                ));
                return;
            }
        };

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Razer Control")
            .default_width(850)
            .default_height(700)
            .build();

        // Close-to-tray: hide window instead of quitting
        window.connect_close_request(|win| {
            win.set_visible(false);
            glib::Propagation::Stop
        });

        let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let header_bar = adw::HeaderBar::new();
        header_bar.set_show_end_title_buttons(true);

        let view_switcher = adw::ViewSwitcher::new();
        view_switcher.set_policy(adw::ViewSwitcherPolicy::Wide);

        let view_stack = adw::ViewStack::new();
        view_switcher.set_stack(Some(&view_stack));
        header_bar.set_title_widget(Some(&view_switcher));

        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(900);
        clamp.set_tightening_threshold(600);

        let scrolled_window = gtk::ScrolledWindow::new();
        scrolled_window.set_vexpand(true);
        scrolled_window.set_hscrollbar_policy(gtk::PolicyType::Never);
        scrolled_window.set_child(Some(&clamp));
        clamp.set_child(Some(&view_stack));

        content_box.append(&header_bar);
        content_box.append(&scrolled_window);

        // Performance page
        let perf_page = make_performance_page(device.clone());
        let page = view_stack.add_titled(&perf_page.page, Some("Performance"), "Performance");
        page.set_icon_name(Some("power-profile-balanced-symbolic"));

        // Lighting page
        let lighting_page = make_lighting_page(device.clone());
        let page = view_stack.add_titled(&lighting_page.page, Some("Lighting"), "Lighting");
        page.set_icon_name(Some("display-brightness-symbolic"));

        // Battery page
        let battery_page = make_battery_page();
        let page = view_stack.add_titled(&battery_page.page, Some("Battery"), "Battery");
        page.set_icon_name(Some("battery-symbolic"));

        // (GPU sections are now part of the Performance page)

        // About page
        let about_page = make_about_page(device.clone());
        let page = view_stack.add_titled(&about_page.page, Some("About"), "About");
        page.set_icon_name(Some("help-about-symbolic"));

        // Separator + status bar at bottom
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        content_box.append(&separator);
        let monitor = create_system_monitor(Arc::clone(&shared_state_for_activate));
        content_box.append(&monitor);

        let toast_overlay = adw::ToastOverlay::new();
        toast_overlay.set_child(Some(&content_box));

        window.set_content(Some(&toast_overlay));
        window.present();

    });

    app.run();
}

// ---------------------------------------------------------------------------
// Performance page
// ---------------------------------------------------------------------------

fn make_performance_page(device: SupportedDevice) -> SettingsPage {
    let settings_page = SettingsPage::new();

    // AC / Battery toggle
    let (toggle_box, is_ac) = make_profile_toggle();
    let refreshing = Rc::new(Cell::new(false));

    // We'll add the toggle to the first group's header
    let toggle_section = settings_page.add_section(None);
    toggle_section.add_row(&toggle_box);

    // --- Power Profile section ---
    let power_section = settings_page.add_section(Some("Power Profile"));

    let initial_ac = is_ac.get();
    let power = get_power(initial_ac);

    let profile_names = power_profile_names(initial_ac);
    let power_combo = make_combo_row(
        "Profile",
        profile_description(power.map_or(if initial_ac { 0 } else { 6 }, |p| p.0 as u32)),
        &profile_names,
        power.map_or(0, |p| profile_wire_to_index(initial_ac, p.0)),
    );
    power_section.add_row(&power_combo);

    let cpu_options: &[&str] = if device.can_boost() {
        &["Low", "Medium", "High", "Boost"]
    } else {
        &["Low", "Medium", "High"]
    };
    let cpu_combo = make_combo_row(
        "CPU Performance",
        "Processor performance level",
        cpu_options,
        power.map_or(0, |p| p.1 as u32),
    );
    power_section.add_row(&cpu_combo);

    let gpu_combo = make_combo_row(
        "GPU Performance",
        "Graphics performance level",
        &["Low", "Medium", "High"],
        power.map_or(0, |p| p.2 as u32),
    );
    power_section.add_row(&gpu_combo);

    let show_boost = power.map_or(false, |p| p.0 == 4);
    cpu_combo.set_visible(show_boost);
    gpu_combo.set_visible(show_boost);

    // --- Cooling section ---
    let fan_section = settings_page.add_section(Some("Cooling"));

    let fan_speed = get_fan_speed(initial_ac).unwrap_or(0);
    let min_fan = *device.fan.get(0).unwrap_or(&0);
    let max_fan = *device.fan.get(1).unwrap_or(&5000);
    let min_fan_speed = min_fan as f64;
    let max_fan_speed = max_fan as f64;

    // Shared smart-curve state for this page (enabled / source / cpu / gpu points).
    let initial_curve = get_fan_curve(initial_ac).unwrap_or_else(comms::FanCurve::new);
    let curve_state = Rc::new(RefCell::new(initial_curve.clone()));

    // Fan is a 3-way choice: Automatic (rpm 0), Manual (fixed rpm), Smart Curve.
    let initial_mode: u32 = if initial_curve.enabled {
        2
    } else if fan_speed == 0 {
        0
    } else {
        1
    };

    let mode_combo = make_combo_row(
        "Fan Mode",
        "Automatic, fixed manual speed, or a smart temperature curve",
        &["Automatic", "Manual", "Smart Curve"],
        initial_mode,
    );
    fan_section.add_row(&mode_combo);

    let fan_slider = SliderRow::new(
        "Fan Speed (RPM)",
        "Manual cooling performance",
        min_fan_speed, max_fan_speed, 100.0,
        if fan_speed > 0 { fan_speed as f64 } else { min_fan_speed },
    );
    fan_slider.add_mark(min_fan_speed, Some("Min"));
    fan_slider.add_mark(max_fan_speed, Some("Max"));
    fan_section.add_row(&fan_slider.container);

    let source_idx: u32 = match initial_curve.source {
        comms::CurveTempSource::Cpu => 0,
        comms::CurveTempSource::Gpu => 1,
        comms::CurveTempSource::Both => 2,
    };
    let source_combo = make_combo_row(
        "Curve Temperature Source",
        "Which temperature drives the fan curve",
        &["CPU", "GPU", "Both"],
        source_idx,
    );
    fan_section.add_row(&source_combo);

    // One editor per zone curve; visibility follows the selected source.
    let cpu_editor = Rc::new(CurveEditor::new(min_fan, max_fan, initial_curve.cpu_points.clone()));
    let gpu_editor = Rc::new(CurveEditor::new(min_fan, max_fan, initial_curve.gpu_points.clone()));

    let cpu_curve_label = gtk::Label::new(Some("CPU Curve  \u{2014}  drag points, click to add, right-click to remove"));
    cpu_curve_label.add_css_class("dim-label");
    cpu_curve_label.add_css_class("caption");
    cpu_curve_label.set_halign(gtk::Align::Start);
    cpu_curve_label.set_margin_start(12);
    cpu_curve_label.set_margin_top(8);
    let gpu_curve_label = gtk::Label::new(Some("GPU Curve  \u{2014}  drag points, click to add, right-click to remove"));
    gpu_curve_label.add_css_class("dim-label");
    gpu_curve_label.add_css_class("caption");
    gpu_curve_label.set_halign(gtk::Align::Start);
    gpu_curve_label.set_margin_start(12);
    gpu_curve_label.set_margin_top(8);

    fan_section.add_row(&cpu_curve_label);
    fan_section.add_row(&cpu_editor.widget);
    fan_section.add_row(&gpu_curve_label);
    fan_section.add_row(&gpu_editor.widget);

    // Visibility controller shared across the mode/source handlers and refresh.
    let update_cooling_visibility: Rc<dyn Fn(u32, u32)> = {
        let fan_container = fan_slider.container.clone();
        let source_combo = source_combo.clone();
        let cpu_editor = cpu_editor.clone();
        let gpu_editor = gpu_editor.clone();
        let cpu_curve_label = cpu_curve_label.clone();
        let gpu_curve_label = gpu_curve_label.clone();
        Rc::new(move |mode: u32, source: u32| {
            fan_container.set_visible(mode == 1);
            source_combo.set_visible(mode == 2);
            let cpu_vis = mode == 2 && (source == 0 || source == 2);
            let gpu_vis = mode == 2 && (source == 1 || source == 2);
            cpu_curve_label.set_visible(cpu_vis);
            cpu_editor.widget.set_visible(cpu_vis);
            gpu_curve_label.set_visible(gpu_vis);
            gpu_editor.widget.set_visible(gpu_vis);
        })
    };
    update_cooling_visibility(initial_mode, source_idx);

    // --- Callbacks ---

    // Set true while the user drags the manual fan slider, so the periodic
    // refresh below does not overwrite the handle with the daemon's stale value.
    let fan_dragging = Rc::new(Cell::new(false));

    // Refresh helper: re-query daemon and update all widgets on this page
    let refresh: Rc<dyn Fn()> = {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let power_combo = power_combo.clone();
        let cpu_combo = cpu_combo.clone();
        let gpu_combo = gpu_combo.clone();
        let mode_combo = mode_combo.clone();
        let source_combo = source_combo.clone();
        let fan_scale = fan_slider.scale.clone();
        let fan_dragging = fan_dragging.clone();
        let update_cooling_visibility = update_cooling_visibility.clone();
        let min_fan = min_fan_speed;
        let fan_group = fan_section.group.clone();
        Rc::new(move || {
            refreshing.set(true);
            let ac = is_ac.get();
            // Synapse parity (user-verified on Synapse 4): fan configuration —
            // manual RPM and the smart curve — is not offered on battery, so
            // the whole Cooling section hides in the DC view. The capability
            // stays in daemon + CLI (`razer-cli write fan bat ...`) as an
            // escape hatch; the EC itself accepts DC zone writes (Synapse
            // performs them on every DC profile switch) — the constraint is a
            // UI-layer rule in Synapse, so it is mirrored at the same layer.
            fan_group.set_visible(ac);
            if let Some(pwr) = get_power(ac) {
                // Rebuild the dropdown for the current domain (AC and battery
                // expose different profiles), then select by wire value.
                set_combo_options(&power_combo, &power_profile_names(ac));
                power_combo.set_selected(profile_wire_to_index(ac, pwr.0));
                power_combo.set_subtitle(profile_description(pwr.0 as u32));
                cpu_combo.set_selected(pwr.1 as u32);
                gpu_combo.set_selected(pwr.2 as u32);
                let show = pwr.0 == 4;
                cpu_combo.set_visible(show);
                gpu_combo.set_visible(show);
            }
            // Poll the fan mode without clobbering in-progress curve edits: only
            // the mode selector, the manual slider value and visibility refresh
            // here. Curve points/source reload on AC switch (reload_curve).
            let fs = get_fan_speed(ac).unwrap_or(0);
            let curve_on = get_fan_curve(ac).map_or(false, |c| c.enabled);
            let mode = if curve_on { 2 } else if fs == 0 { 0 } else { 1 };
            mode_combo.set_selected(mode);
            // Don't fight an in-progress drag: the release handler will commit
            // and the next refresh will then reflect the new value.
            if !fan_dragging.get() {
                if mode == 1 {
                    fan_scale.set_value(fs as f64);
                } else if mode == 0 {
                    fan_scale.set_value(min_fan);
                }
            }
            update_cooling_visibility(mode, source_combo.selected());
            refreshing.set(false);
        })
    };

    // Reload the full curve (source + points) for the current AC state. Used on
    // AC/Battery switch, where replacing the editor contents is intended.
    let reload_curve: Rc<dyn Fn()> = {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let curve_state = curve_state.clone();
        let source_combo = source_combo.clone();
        let cpu_editor = cpu_editor.clone();
        let gpu_editor = gpu_editor.clone();
        Rc::new(move || {
            if let Some(curve) = get_fan_curve(is_ac.get()) {
                refreshing.set(true);
                source_combo.set_selected(match curve.source {
                    comms::CurveTempSource::Cpu => 0,
                    comms::CurveTempSource::Gpu => 1,
                    comms::CurveTempSource::Both => 2,
                });
                cpu_editor.set_points(curve.cpu_points.clone());
                gpu_editor.set_points(curve.gpu_points.clone());
                *curve_state.borrow_mut() = curve;
                refreshing.set(false);
            }
        })
    };

    // Toggle callback — hook the actual toggle buttons to refresh page state
    {
        let first_child = toggle_box.first_child();
        if let Some(ac_btn) = first_child {
            if let Ok(tb) = ac_btn.downcast::<gtk::ToggleButton>() {
                let refresh = refresh.clone();
                let reload_curve = reload_curve.clone();
                tb.connect_toggled(move |btn| {
                    if btn.is_active() {
                        reload_curve();
                        refresh();
                    }
                });
            }
        }
        let last_child = toggle_box.last_child();
        if let Some(bat_btn) = last_child {
            if let Ok(tb) = bat_btn.downcast::<gtk::ToggleButton>() {
                let refresh = refresh.clone();
                let reload_curve = reload_curve.clone();
                tb.connect_toggled(move |btn| {
                    if btn.is_active() {
                        reload_curve();
                        refresh();
                    }
                });
            }
        }
    }

    // Power profile change
    {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let cpu_combo = cpu_combo.clone();
        let gpu_combo = gpu_combo.clone();
        power_combo.connect_selected_notify(glib::clone!(
            #[weak] cpu_combo, #[weak] gpu_combo,
            move |pp| {
                if refreshing.get() { return; }
                let ac = is_ac.get();
                let wire = profile_index_to_wire(ac, pp.selected());
                let cpu = cpu_combo.selected() as u8;
                let gpu = gpu_combo.selected() as u8;
                set_power(ac, (wire, cpu, gpu));
                pp.set_subtitle(profile_description(wire as u32));
                let show = wire == 4;
                cpu_combo.set_visible(show);
                gpu_combo.set_visible(show);
            }
        ));
    }

    {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let power_combo = power_combo.clone();
        let gpu_combo = gpu_combo.clone();
        cpu_combo.connect_selected_notify(glib::clone!(
            #[weak] power_combo, #[weak] gpu_combo,
            move |cb| {
                if refreshing.get() { return; }
                let ac = is_ac.get();
                let wire = profile_index_to_wire(ac, power_combo.selected());
                let cpu = cb.selected() as u8;
                let gpu = gpu_combo.selected() as u8;
                set_power(ac, (wire, cpu, gpu));
            }
        ));
    }

    {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let power_combo = power_combo.clone();
        let cpu_combo = cpu_combo.clone();
        gpu_combo.connect_selected_notify(glib::clone!(
            #[weak] power_combo, #[weak] cpu_combo,
            move |gb| {
                if refreshing.get() { return; }
                let ac = is_ac.get();
                let wire = profile_index_to_wire(ac, power_combo.selected());
                let cpu = cpu_combo.selected() as u8;
                let gpu = gb.selected() as u8;
                set_power(ac, (wire, cpu, gpu));
            }
        ));
    }

    // Fan slider (Manual mode): commit the fixed RPM when the drag ends.
    {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let fan_dragging = fan_dragging.clone();
        connect_commit_on_release(&fan_slider.scale, refreshing, fan_dragging, move |value| {
            set_fan_speed(is_ac.get(), value as i32);
        });
    }

    // Fan mode selector: Automatic / Manual / Smart Curve.
    {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let curve_state = curve_state.clone();
        let source_combo = source_combo.clone();
        let fan_scale = fan_slider.scale.clone();
        let update_cooling_visibility = update_cooling_visibility.clone();
        mode_combo.connect_selected_notify(move |c| {
            if refreshing.get() { return; }
            let ac = is_ac.get();
            let mode = c.selected();
            match mode {
                0 => { set_fan_speed(ac, 0); }
                1 => {
                    let value = fan_scale.value().max(min_fan_speed);
                    fan_scale.set_value(value);
                    set_fan_speed(ac, value as i32);
                }
                2 => {
                    let snapshot = {
                        let mut curve = curve_state.borrow_mut();
                        curve.enabled = true;
                        curve.clone()
                    };
                    set_fan_curve(ac, snapshot);
                }
                _ => {}
            }
            update_cooling_visibility(mode, source_combo.selected());
        });
    }

    // Curve temperature source.
    {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let curve_state = curve_state.clone();
        let update_cooling_visibility = update_cooling_visibility.clone();
        source_combo.connect_selected_notify(move |c| {
            if refreshing.get() { return; }
            let source = match c.selected() {
                1 => comms::CurveTempSource::Gpu,
                2 => comms::CurveTempSource::Both,
                _ => comms::CurveTempSource::Cpu,
            };
            let snapshot = {
                let mut curve = curve_state.borrow_mut();
                curve.source = source;
                curve.enabled = true;
                curve.clone()
            };
            set_fan_curve(is_ac.get(), snapshot);
            update_cooling_visibility(2, c.selected());
        });
    }

    // Curve editors persist their zone's points on every edit.
    {
        let is_ac = is_ac.clone();
        let curve_state = curve_state.clone();
        cpu_editor.connect_changed(move |pts| {
            let snapshot = {
                let mut curve = curve_state.borrow_mut();
                curve.cpu_points = pts;
                curve.enabled = true;
                curve.clone()
            };
            set_fan_curve(is_ac.get(), snapshot);
        });
    }
    {
        let is_ac = is_ac.clone();
        let curve_state = curve_state.clone();
        gpu_editor.connect_changed(move |pts| {
            let snapshot = {
                let mut curve = curve_state.borrow_mut();
                curve.gpu_points = pts;
                curve.enabled = true;
                curve.clone()
            };
            set_fan_curve(is_ac.get(), snapshot);
        });
    }

    // -----------------------------------------------------------------------
    // Live-sync: keep the performance combos in step with external changes
    // (powerkey cycling, CLI) every 2 s — only while the page is actually
    // visible (Stack pages unmap when not shown; close-to-tray unmaps all).
    // -----------------------------------------------------------------------
    {
        let refresh = refresh.clone();
        let page = settings_page.page.clone();
        glib::timeout_add_local(Duration::from_secs(2), move || {
            if !page.is_mapped() {
                return glib::ControlFlow::Continue;
            }
            refresh();
            glib::ControlFlow::Continue
        });
    }

    settings_page
}

// ---------------------------------------------------------------------------
// Lighting page
// ---------------------------------------------------------------------------

fn make_lighting_page(device: SupportedDevice) -> SettingsPage {
    let settings_page = SettingsPage::new();

    // AC / Battery toggle (affects brightness + logo only)
    let (toggle_box, is_ac) = make_profile_toggle();
    let refreshing = Rc::new(Cell::new(false));

    let toggle_section = settings_page.add_section(None);
    toggle_section.add_row(&toggle_box);

    // --- Keyboard Brightness ---
    let brightness_section = settings_page.add_section(Some("Keyboard Brightness"));

    let initial_ac = is_ac.get();
    let brightness = get_brightness(initial_ac).unwrap_or(100);

    let brightness_slider = SliderRow::new(
        "Brightness Level",
        "Adjust keyboard backlight intensity",
        0.0, 100.0, 1.0,
        brightness as f64,
    );
    brightness_slider.add_mark(0.0, Some("Off"));
    brightness_slider.add_mark(50.0, Some("50%"));
    brightness_slider.add_mark(100.0, Some("100%"));
    brightness_section.add_row(&brightness_slider.container);

    // --- Logo (conditional) ---
    let logo_combo: Option<adw::ComboRow> = if device.has_logo() {
        let logo_section = settings_page.add_section(Some("Logo"));
        let logo = get_logo(initial_ac).unwrap_or(1);
        let combo = make_combo_row(
            "Logo Mode",
            "Control Razer logo lighting",
            &["Off", "On", "Breathing"],
            logo as u32,
        );
        logo_section.add_row(&combo);
        Some(combo)
    } else {
        None
    };

    // --- Keyboard Effects (GLOBAL — not affected by AC/Battery toggle) ---
    let effects_section = settings_page.add_section(Some("Keyboard Effects"));

    let effect_combo = make_combo_row(
        "Effect Type",
        "Choose keyboard lighting effect",
        &["Static", "Static Gradient", "Wave Gradient", "Breathing"],
        0,
    );
    effects_section.add_row(&effect_combo);

    let color1 = ColorRow::new("Primary Color", "Select the main color");
    effects_section.add_row(&color1.row);

    let color2 = ColorRow::new("Secondary Color", "For gradient effects");
    color2.row.set_visible(false); // hidden by default (Static has no gradient)
    effects_section.add_row(&color2.row);

    // Restore saved effect selection and colors from config
    if let Some((effect_idx, params)) = get_standard_effect() {
        if effect_idx <= 3 {
            effect_combo.set_selected(effect_idx as u32);
            color2.row.set_visible(effect_idx == 1 || effect_idx == 2);
        }
        if params.len() >= 3 {
            let rgba = gtk::gdk::RGBA::new(
                params[0] as f32 / 255.0,
                params[1] as f32 / 255.0,
                params[2] as f32 / 255.0,
                1.0,
            );
            color1.button.set_rgba(&rgba);
        }
        if params.len() >= 6 {
            let rgba = gtk::gdk::RGBA::new(
                params[3] as f32 / 255.0,
                params[4] as f32 / 255.0,
                params[5] as f32 / 255.0,
                1.0,
            );
            color2.button.set_rgba(&rgba);
        }
    }

    // Show/hide secondary color based on effect
    {
        let color2_row = color2.row.clone();
        effect_combo.connect_selected_notify(move |c| {
            let idx = c.selected();
            // Gradient effects: index 1 (Static Gradient) and 2 (Wave Gradient)
            color2_row.set_visible(idx == 1 || idx == 2);
        });
    }

    // Apply button
    let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    button_box.set_margin_top(12);
    button_box.set_margin_bottom(12);
    button_box.set_margin_start(12);
    button_box.set_margin_end(12);
    button_box.set_halign(gtk::Align::End);

    let apply_button = gtk::Button::with_label("Apply Effect");
    apply_button.add_css_class("suggested-action");
    button_box.append(&apply_button);
    effects_section.add_row(&button_box);

    {
        let effect_ref = effect_combo.clone();
        let color1_btn = color1.button.clone();
        let color2_btn = color2.button.clone();
        apply_button.connect_clicked(move |btn| {
            let c1 = color1_btn.rgba();
            let red = (c1.red() * 255.0) as u8;
            let green = (c1.green() * 255.0) as u8;
            let blue = (c1.blue() * 255.0) as u8;

            let c2 = color2_btn.rgba();
            let red2 = (c2.red() * 255.0) as u8;
            let green2 = (c2.green() * 255.0) as u8;
            let blue2 = (c2.blue() * 255.0) as u8;

            let ok = match effect_ref.selected() {
                0 => set_effect("static", vec![red, green, blue]),
                1 => set_effect("static_gradient", vec![red, green, blue, red2, green2, blue2]),
                2 => set_effect("wave_gradient", vec![red, green, blue, red2, green2, blue2]),
                3 => set_effect("breathing_single", vec![red, green, blue, 10]),
                _ => None,
            };

            // Show toast feedback
            if let Some(root) = btn.root() {
                if let Some(window) = root.downcast_ref::<adw::ApplicationWindow>() {
                    if let Some(child) = window.content() {
                        if let Ok(overlay) = child.downcast::<adw::ToastOverlay>() {
                            let toast = if ok == Some(true) {
                                adw::Toast::new("Effect applied")
                            } else {
                                adw::Toast::new("Failed to apply effect")
                            };
                            toast.set_timeout(2);
                            overlay.add_toast(toast);
                        }
                    }
                }
            }
        });
    }

    // --- Callbacks for AC/Battery toggle (brightness + logo only) ---

    let brightness_dragging = Rc::new(Cell::new(false));

    let refresh = {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let brightness_scale = brightness_slider.scale.clone();
        let brightness_dragging = brightness_dragging.clone();
        let logo_combo = logo_combo.clone();
        move || {
            refreshing.set(true);
            let ac = is_ac.get();
            let br = get_brightness(ac).unwrap_or(100);
            if !brightness_dragging.get() {
                brightness_scale.set_value(br as f64);
            }
            if let Some(ref lc) = logo_combo {
                let logo = get_logo(ac).unwrap_or(1);
                lc.set_selected(logo as u32);
            }
            refreshing.set(false);
        }
    };

    // Hook toggle buttons for refresh
    {
        let first_child = toggle_box.first_child();
        if let Some(ac_btn) = first_child {
            if let Ok(tb) = ac_btn.downcast::<gtk::ToggleButton>() {
                let refresh = refresh.clone();
                tb.connect_toggled(move |btn| {
                    if btn.is_active() {
                        refresh();
                    }
                });
            }
        }
        let last_child = toggle_box.last_child();
        if let Some(bat_btn) = last_child {
            if let Ok(tb) = bat_btn.downcast::<gtk::ToggleButton>() {
                let refresh = refresh.clone();
                tb.connect_toggled(move |btn| {
                    if btn.is_active() {
                        refresh();
                    }
                });
            }
        }
    }

    // Brightness change
    {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        let brightness_dragging = brightness_dragging.clone();
        connect_commit_on_release(&brightness_slider.scale, refreshing, brightness_dragging, move |value| {
            set_brightness(is_ac.get(), value as u8);
        });
    }

    // Logo change
    if let Some(ref lc) = logo_combo {
        let is_ac = is_ac.clone();
        let refreshing = refreshing.clone();
        lc.connect_selected_notify(move |c| {
            if refreshing.get() { return; }
            let ac = is_ac.get();
            let logo = c.selected() as u8;
            set_logo(ac, logo);
        });
    }

    // Live-sync: poll daemon every 2s so widget changes appear in GUI
    {
        let refresh = refresh.clone();
        glib::timeout_add_local(Duration::from_secs(2), move || {
            refresh();
            glib::ControlFlow::Continue
        });
    }

    settings_page
}

// ---------------------------------------------------------------------------
// Battery page
// ---------------------------------------------------------------------------

fn make_battery_page() -> SettingsPage {
    let page = SettingsPage::new();

    let bho = get_bho();

    if let Some(bho) = bho {
        let refreshing = Rc::new(Cell::new(false));
        let section = page.add_section(Some("Battery Health Optimizer"));

        let bho_switch = make_switch_row(
            "Limit Charging",
            "Cap maximum charge to extend battery lifespan",
            bho.0,
        );
        section.add_row(&bho_switch);

        let bho_slider = SliderRow::new(
            "Charge Limit",
            "Maximum battery charge level (%)",
            50.0, 80.0, 5.0,
            bho.1 as f64,
        );
        bho_slider.add_mark(50.0, Some("50%"));
        bho_slider.add_mark(65.0, Some("65%"));
        bho_slider.add_mark(80.0, Some("80%"));
        bho_slider.scale.set_sensitive(bho.0);
        section.add_row(&bho_slider.container);

        let bho_dragging = Rc::new(Cell::new(false));

        {
            let bho_switch_ref = bho_switch.clone();
            let refreshing = refreshing.clone();
            let bho_dragging = bho_dragging.clone();
            connect_commit_on_release(&bho_slider.scale, refreshing, bho_dragging, move |value| {
                set_bho(bho_switch_ref.is_active(), value as u8);
            });
        }

        {
            let refreshing = refreshing.clone();
            let scale_ref = bho_slider.scale.clone();
            bho_switch.connect_active_notify(glib::clone!(
                #[weak] scale_ref,
                move |sw| {
                    if refreshing.get() { return; }
                    let state = sw.is_active();
                    let threshold = scale_ref.value() as u8;
                    set_bho(state, threshold);
                    scale_ref.set_sensitive(state);
                }
            ));
        }

        // Live-sync: poll daemon every 2s so widget changes appear in GUI
        {
            let bho_switch = bho_switch.clone();
            let bho_scale = bho_slider.scale.clone();
            let bho_dragging = bho_dragging.clone();
            glib::timeout_add_local(Duration::from_secs(2), move || {
                if let Some((is_on, threshold)) = get_bho() {
                    refreshing.set(true);
                    bho_switch.set_active(is_on);
                    if !bho_dragging.get() {
                        bho_scale.set_value(threshold as f64);
                    }
                    bho_scale.set_sensitive(is_on);
                    refreshing.set(false);
                }
                glib::ControlFlow::Continue
            });
        }
    } else {
        let status = adw::StatusPage::new();
        status.set_icon_name(Some("battery-symbolic"));
        status.set_title("Not Available");
        status.set_description(Some("Battery health optimizer is not supported on this device."));
        page.page.add(&adw::PreferencesGroup::new());
        let section = page.add_section(None);
        section.add_row(&status);
    }

    page
}




// ---------------------------------------------------------------------------
// About page
// ---------------------------------------------------------------------------

fn make_about_page(device: SupportedDevice) -> SettingsPage {
    let page = SettingsPage::new();

    // Application Info Section
    let section = page.add_section(Some("Application"));

    let app_name = gtk::Label::new(Some("Razer Control (Revived)"));
    app_name.add_css_class("title-2");
    let row = SettingsRow::new("Name", &app_name);
    section.add_row(&row.row);

    // Single-sourced from Cargo.toml at build time (CARGO_PKG_VERSION) — the
    // GUI, `razer-cli --version` and the package version cannot diverge again.
    let version_label = gtk::Label::new(Some(concat!(
        "v",
        env!("CARGO_PKG_VERSION"),
        " \u{00B7} Blade 16 2025 personal fork"
    )));
    let row = SettingsRow::new("Version", &version_label);
    row.set_subtitle("Based on the wsquarepa fork (upstream 0.3.0-rc4 line)");
    section.add_row(&row.row);

    let url = gtk::LinkButton::with_label(
        "https://github.com/encomjp/razer-control-revived",
        "View on GitHub",
    );
    let row = SettingsRow::new("Repository", &url);
    row.set_subtitle("Report issues and contribute");
    section.add_row(&row.row);

    let fork_url = gtk::LinkButton::with_label(
        "https://github.com/wsquarepa/razer-control-revived",
        "View on GitHub",
    );
    let row = SettingsRow::new("Fork base", &fork_url);
    row.set_subtitle("The fork this build is based on");
    section.add_row(&row.row);

    // Device Information Section

    // Device Information Section
    let section = page.add_section(Some("Device Information"));

    let name_label = gtk::Label::new(Some(&device.name));
    name_label.set_wrap(true);
    let row = SettingsRow::new("Model", &name_label);
    row.set_subtitle("Detected Razer laptop model");
    section.add_row(&row.row);

    let features = device.features.join(", ");
    let features_label = gtk::Label::new(Some(&features));
    features_label.set_wrap(true);
    let row = SettingsRow::new("Features", &features_label);
    row.set_subtitle("Supported hardware capabilities");
    section.add_row(&row.row);

    let fan_min = device.fan.get(0).unwrap_or(&0);
    let fan_max = device.fan.get(1).unwrap_or(&5000);
    let fan_range = format!("{} - {} RPM", fan_min, fan_max);
    let fan_label = gtk::Label::new(Some(&fan_range));
    let row = SettingsRow::new("Fan Range", &fan_label);
    row.set_subtitle("Minimum to maximum fan speed");
    section.add_row(&row.row);

    // About Section
    let section = page.add_section(Some("About"));

    let desc_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    desc_box.set_margin_top(12);
    desc_box.set_margin_bottom(12);
    desc_box.set_margin_start(12);
    desc_box.set_margin_end(12);

    let description = gtk::Label::new(Some(
        "Open-source control center for Razer laptops on Linux.\n\
        Manage power profiles, fan speeds, keyboard lighting, and more.\n\n\
        \u{26A0}\u{FE0F} Tested on: Fedora & Arch Linux\n\
        Should work on Ubuntu and similar distributions.\n\
        If issues occur, please report them on GitHub."
    ));
    description.set_wrap(true);
    description.set_justify(gtk::Justification::Center);
    description.add_css_class("dim-label");
    desc_box.append(&description);
    section.add_row(&desc_box);
    page
}

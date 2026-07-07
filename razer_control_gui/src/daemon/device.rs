// mod kbd;
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;
use std::{thread, time, io, fs};
use std::ffi::CString;
use hidapi::HidApi;
use crate::dbus_mutter_idlemonitor;
use crate::config;
use crate::battery;
use crate::comms::{CurveTempSource, FanCurve, FanCurvePoint};
use dbus::blocking::Connection;

const RAZER_VENDOR_ID: u16 = 0x1532;

#[derive(Serialize, Deserialize, Debug)]
pub struct SupportedDevice {
    pub name: String,
    pub vid: String,
    pub pid: String,
    pub features: Vec<String>,
    pub fan: Vec<u16>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RazerPacket {
    report: u8,
    status: u8,
    id: u8,
    remaining_packets: u16,
    protocol_type: u8,
    data_size: u8,
    command_class: u8,
    command_id: u8,
    #[serde(with = "BigArray")]
    args: [u8; 80],
    crc: u8,
    reserved: u8,
}

impl RazerPacket {
// Command status
    const RAZER_CMD_NEW:u8 = 0x00;
    const RAZER_CMD_BUSY:u8 = 0x01;
    const RAZER_CMD_SUCCESSFUL:u8 = 0x02;
    const RAZER_CMD_FAILURE:u8 = 0x03;
    const RAZER_CMD_TIMEOUT:u8 = 0x04;
    const RAZER_CMD_NOT_SUPPORTED:u8 = 0x05;

    fn new(command_class: u8, command_id: u8, data_size: u8) -> RazerPacket {
        return RazerPacket {
            report: 0x00,
            status: RazerPacket::RAZER_CMD_NEW,
            id: 0x1F,
            remaining_packets: 0x0000,
            protocol_type: 0x00,
            data_size,
            command_class,
            command_id,
            args: [0x00; 80],
            crc: 0x00,
            reserved: 0x00,
        };
    }

    fn calc_crc(&mut self) -> Vec<u8>{
        let mut buf: Vec<u8> = bincode::serialize(self).unwrap();
        // Razer CRC = XOR of the 90-byte report's bytes [2..88). Index 0 of this struct
        // is the prepended HID report-id, so that range maps to buf[3..89]; the crc byte
        // itself sits at buf[89].
        let mut res: u8 = 0x00;
        for i in 3..89 {
            res ^= buf[i];
        }
        self.crc = res;
        buf[89] = res;
        return buf;
    }
}

fn device_file_path() -> String {
    std::env::var("RAZER_DEVICE_FILE")
        .unwrap_or_else(|_| "/usr/share/razercontrol/laptops.json".to_string())
}

pub struct DeviceManager {
    pub device: Option <RazerLaptop>,
    supported_devices: Vec<SupportedDevice>,
    pub config: Option <config::Configuration>,
    pub idle_id: u32,
    pub active_id: u32,
    add_active: bool,
    pub change_idle: bool,
    /// Whether the EC is currently latched into manual fan mode by the curve
    /// task. Cleared whenever something else may have reset the EC (power-mode
    /// change, AC switch, resume) so the next curve tick re-asserts manual mode.
    fan_curve_established: bool,
    /// Last RPM the curve task wrote to both fan zones, to skip redundant writes.
    fan_curve_last_rpm: Option<u16>,
}

impl DeviceManager {
    /// Read the USB interface number for a /dev/hidrawX node from sysfs.
    fn hidraw_iface_number(hidraw_name: &str) -> Option<i32> {
        let iface_path = format!("/sys/class/hidraw/{}/device/../bInterfaceNumber", hidraw_name);
        let raw = fs::read_to_string(iface_path).ok()?;
        i32::from_str_radix(raw.trim(), 16).ok()
    }

    fn read_hex_u16(path: &std::path::Path) -> Option<u16> {
        let raw = fs::read_to_string(path).ok()?;
        let trimmed = raw.trim();
        u16::from_str_radix(trimmed, 16).ok()
    }

    /// Resolve VID/PID for a /dev/hidrawX node via /sys, walking up parents
    /// until we find idVendor/idProduct.
    fn hidraw_vid_pid(hidraw_name: &str) -> Option<(u16, u16)> {
        let mut current = fs::canonicalize(format!("/sys/class/hidraw/{}/device", hidraw_name)).ok()?;

        for _ in 0..6 {
            let vid_path = current.join("idVendor");
            let pid_path = current.join("idProduct");
            if vid_path.exists() && pid_path.exists() {
                let vid = Self::read_hex_u16(&vid_path)?;
                let pid = Self::read_hex_u16(&pid_path)?;
                return Some((vid, pid));
            }
            if !current.pop() {
                break;
            }
        }
        None
    }

    pub fn new () -> DeviceManager {
        return DeviceManager {
            device: None,
            supported_devices: vec![],
            config: None,
            idle_id: 0,
            active_id: 0,
            add_active: false,
            change_idle: false,
            fan_curve_established: false,
            fan_curve_last_rpm: None,
        };
    }

    pub fn add_idle_watch(&mut self, proxy_idle: &dyn dbus_mutter_idlemonitor::OrgGnomeMutterIdleMonitor) {
        if self.change_idle {
            let mut timeout: u64 = 0;
            let mut state: usize = 0;
            if let Some(laptop) = self.get_device() {
                state = laptop.get_ac_state();
            }
            if let Some(config) = self.get_config() {
                timeout = config.power[state].idle as u64 * 60 * 1000; // idle is in minutes timeout is in miliseconds
            }
            if timeout != 0 {
                if self.idle_id != 0 {
                    self.remove_watch(proxy_idle);
                }
                if let Ok(id) = proxy_idle.add_idle_watch(timeout) {
                    println!("idle handler {:?}", id);
                    self.idle_id = id;
                }
            } else {
                if self.idle_id != 0 {
                    self.remove_watch(proxy_idle);
                }
            }
            self.change_idle = false;
        }
    }

    pub fn set_sync(&mut self, sync: bool) -> bool {
        let mut ac: usize = 0;
        if let Some(laptop) = self.get_device() {
            ac = laptop.ac_state as usize;
        }
        let other = (ac + 1) & 0x01;
        if let Some(config) = self.get_config() {
            config.sync = sync;
            config.power[other].brightness = config.power[ac].brightness;
            config.power[other].logo_state = config.power[ac].logo_state;
            config.power[other].screensaver = config.power[ac].screensaver;
            config.power[other].idle = config.power[ac].idle;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }

        return true;
    }

    pub fn get_sync(&mut self) -> bool {
        if let Some(config) = self.get_config() {
            return config.sync;
        }

        return false;
    }

    fn remove_watch(&mut self, proxy_idle: &dyn dbus_mutter_idlemonitor::OrgGnomeMutterIdleMonitor) {
        if let Ok(_) = proxy_idle.remove_watch(self.idle_id) {
            println!("remove idle handler");
        }
    }

    pub fn add_active_watch(&mut self, proxy_idle: &dyn dbus_mutter_idlemonitor::OrgGnomeMutterIdleMonitor) {
        if self.add_active {
            if let Ok(id) = proxy_idle.add_user_active_watch() {
                println!("active handler {:?}", id);
                self.active_id = id;
            }
        }
    }

    pub fn read_laptops_file() -> io::Result<DeviceManager > {
        let path = device_file_path();
        let str: Vec<u8> = fs::read(&path)?;
        let mut res: DeviceManager = DeviceManager::new();
        res.supported_devices = serde_json::from_slice(str.as_slice())?;
        println!("suported devices found: {:?}", res.supported_devices.len());
        match config::Configuration::read_from_config() {
            Ok(c) => res.config = Some(c),
            Err(_) => res.config = Some(config::Configuration::new()),
        }

        Ok(res)
    }

    fn get_ac_config(&mut self, ac: usize) -> Option<config::PowerConfig> {
        if let Some(c) = self.get_config() {
            return Some(c.power[ac].clone());
        }

        return None;
    }

    pub fn light_off(&mut self) {
        if self.idle_id != 0 {
            self.add_active = true;
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_screensaver(true);
            laptop.set_brightness(0);
            laptop.set_logo_led_state(0);
        }
    }

    pub fn restore_light(&mut self) {
        self.add_active = false;
        let mut brightness = 0;
        let mut logo_state = 0;
        let mut ac:usize = 0;
        if let Some(laptop) = self.get_device() {
            ac = laptop.get_ac_state();
        }
        if let Some(config) = self.get_ac_config(ac) {
            brightness = config.brightness;
            logo_state = config.logo_state;
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_screensaver(false);
            laptop.set_brightness(brightness);
            laptop.set_logo_led_state(logo_state);
        }
    }

    /// Check whether the current device declares a given feature.
    pub fn device_has_feature(&self, feature: &str) -> bool {
        self.device.as_ref().map_or(false, |d| d.features.contains(&feature.to_string()))
    }

    pub fn restore_standard_effect(&mut self) {
        let mut effect = 0;
        let mut params: Vec<u8> = vec![];
        if let Some(config) = self.get_config() {
            effect = config.standard_effect;
            params = config.standard_effect_params.clone();
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_standard_effect(effect, params);
        }
    }

    pub fn change_idle(&mut self, ac: usize, timeout: u32) -> bool {
        // let mut arm: bool = false;
        if let Some(config) = self.get_config() {
            if config.power[ac].idle != timeout {
                config.power[ac].idle = timeout;
                if config.sync {
                    let other = (ac + 1) & 0x01;
                    config.power[other].idle = timeout;
                }
                if let Err(e) = config.write_to_file() {
                    eprintln!("Error write config {:?}", e);
                }
                // arm = true;
                self.change_idle = true;
            }
        }

        return true;
    }

    /// Re-apply (to hardware only, no config write) the saved power mode for the
    /// current AC state. Used to re-latch GPU boost when the dGPU resumes.
    pub fn reapply_power_mode(&mut self) -> bool {
        // Re-applying a power mode rewrites the per-zone fan-state command, so
        // the curve must re-assert manual mode afterwards.
        self.fan_curve_established = false;
        let ac = match self.get_device() {
            Some(laptop) => laptop.get_ac_state(),
            None => return false,
        };
        let config = match self.get_ac_config(ac) {
            Some(config) => config,
            None => return false,
        };
        println!(
            "Re-applying power profile: ac={} power_mode={} cpu_boost={} gpu_boost={}",
            ac, config.power_mode, config.cpu_boost, config.gpu_boost
        );
        let result = match self.get_device() {
            Some(laptop) => laptop.set_power_mode(config.power_mode, config.cpu_boost, config.gpu_boost),
            None => false,
        };
        // The power-mode reapply rewrote the fan-state command; re-latch the curve
        // now instead of waiting for the next tick (avoids the resume-burst rattle).
        self.reassert_fan_curve();
        result
    }

    pub fn set_power_mode(&mut self, ac: usize, pwr: u8, cpu: u8, gpu: u8) -> bool {
        let mut res: bool = false;
        // The power-mode command rewrites the per-zone fan-state, so re-assert
        // manual fan mode on the next curve tick.
        self.fan_curve_established = false;
        if let Some(config) = self.get_config() {
            config.power[ac].power_mode = pwr;
            config.power[ac].cpu_boost = cpu;
            config.power[ac].gpu_boost = gpu;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if state != ac {
                res = true;
            } else {
                res = laptop.set_power_mode(pwr, cpu, gpu);
            }
        }

        self.reassert_fan_curve();
        return res;
    }

    pub fn get_standard_effect(&mut self) -> (u8, Vec<u8>) {
        if let Some(config) = self.get_config() {
            return (config.gui_effect, config.gui_effect_params.clone());
        }
        return (0, vec![]);
    }

    pub fn save_gui_effect(&mut self, effect_idx: u8, params: Vec<u8>) {
        if let Some(config) = self.get_config() {
            config.gui_effect = effect_idx;
            config.gui_effect_params = params;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }
    }

    pub fn set_standard_effect(&mut self, effect_id: u8, params: Vec<u8>) -> bool {
        if let Some(config) = self.get_config() {
            config.standard_effect = effect_id;
            config.standard_effect_params = params.clone();
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_standard_effect(effect_id, params);
        }

        return true;
    }

    pub fn set_fan_rpm(&mut self, ac:usize, rpm: i32) -> bool {
        // Validate at the daemon boundary: 0 = auto, anything else must be a
        // plausible RPM inside the model's range. The old path cast i32 -> u16
        // unchecked, so -1 wrapped to 65535 (clamped to MAX fans) and 70000
        // wrapped to a silently wrong speed. Reject instead of reinterpret.
        let range_ok = self
            .device
            .as_ref()
            .map_or(false, |d| rpm >= d.fan[0] as i32 && rpm <= d.fan[1] as i32);
        if rpm != 0 && !range_ok {
            eprintln!("Rejected fan rpm {} (valid: 0 for auto, or the model range)", rpm);
            return false;
        }
        let mut res: bool = false;
        // Auto/manual-fixed and the smart curve are mutually exclusive fan modes;
        // selecting a fixed RPM (or auto) turns the curve off for this AC state.
        self.fan_curve_established = false;
        self.fan_curve_last_rpm = None;
        if let Some(config) = self.get_config() {
            config.power[ac].fan_rpm = rpm;
            config.power[ac].fan_curve.enabled = false;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }

        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if state != ac {
                res = true;
            } else {
                res = laptop.set_fan_rpm(rpm as u16);
            }
        }

        return res;
    }

    pub fn set_fan_curve(&mut self, ac: usize, curve: FanCurve) -> bool {
        // Re-evaluate from scratch on the next tick (mode + RPM may both change).
        self.fan_curve_established = false;
        self.fan_curve_last_rpm = None;
        let enabled = curve.enabled;
        let fan_rpm: i32;
        if let Some(config) = self.get_config() {
            config.power[ac].fan_curve = curve;
            fan_rpm = config.power[ac].fan_rpm;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
                return false;
            }
        } else {
            return false;
        }
        // Turning the curve off hands the fans back to this state's saved
        // auto/manual setting so they don't stay pinned at the last curve RPM.
        if !enabled {
            if let Some(laptop) = self.get_device() {
                if laptop.get_ac_state() == ac {
                    laptop.set_fan_rpm(fan_rpm as u16);
                }
            }
        }
        return true;
    }

    pub fn get_fan_curve(&mut self, ac: usize) -> FanCurve {
        if let Some(config) = self.get_ac_config(ac) {
            return config.fan_curve;
        }
        return FanCurve::new();
    }

    /// Returns the active curve's temperature source for the *current* AC state,
    /// or `None` when no curve is enabled. Used by the daemon task to decide
    /// which temperatures to read before driving the fans.
    pub fn active_fan_curve_source(&mut self) -> Option<CurveTempSource> {
        let ac = self.get_device().map(|l| l.get_ac_state())?;
        let config = self.get_ac_config(ac)?;
        if config.fan_curve.enabled {
            Some(config.fan_curve.source)
        } else {
            None
        }
    }

    /// One iteration of the smart fan-curve control loop. Reads the current AC
    /// state's curve, resolves a target RPM from the supplied temperatures and
    /// drives both fan zones. On the first tick after the curve becomes the
    /// authority it latches manual mode (with a settle delay) before writing RPM;
    /// at steady state it only writes RPM, and skips the write entirely when the
    /// target is unchanged.
    pub fn fan_curve_tick(&mut self, cpu_temp: Option<f64>, gpu_temp: Option<f64>) {
        let ac = match self.get_device() {
            Some(laptop) => laptop.get_ac_state(),
            None => return,
        };
        let curve = match self.get_ac_config(ac) {
            Some(config) => config.fan_curve,
            None => return,
        };
        if !curve.enabled {
            self.fan_curve_established = false;
            self.fan_curve_last_rpm = None;
            return;
        }

        let target = match compute_curve_rpm(&curve, cpu_temp, gpu_temp) {
            Some(rpm) => rpm,
            None => return, // no usable temperature this tick; leave fans as-is
        };

        let need_establish = !self.fan_curve_established;
        if !need_establish && self.fan_curve_last_rpm == Some(target) {
            return;
        }

        if let Some(laptop) = self.get_device() {
            if need_establish {
                // set_fan_manual issues two 0x0d/0x02 writes; send_report already
                // settles 200ms after each, latching manual mode before the speed
                // writes below. No additional sleep needed here.
                laptop.set_fan_manual();
            }
            laptop.set_zone_rpm(0x01, target);
            laptop.set_zone_rpm(0x02, target);
        }
        self.fan_curve_established = true;
        self.fan_curve_last_rpm = Some(target);
    }

    /// Re-latch an active smart fan curve immediately after a code path rewrote
    /// the EC fan-state command (power-mode reapply, AC/profile switch, resume).
    /// Without this the curve only re-asserts on its next periodic tick (up to
    /// FAN_CURVE_POLL_SECS later); during the dGPU-resume reapply burst that
    /// deferred window is re-opened every ~2s, whipsawing the fans between the
    /// firmware auto curve and the manual target. Reuses the last computed target
    /// so no fresh temperature read is needed.
    fn reassert_fan_curve(&mut self) {
        let ac = match self.get_device() {
            Some(laptop) => laptop.get_ac_state(),
            None => return,
        };
        let enabled = self
            .get_ac_config(ac)
            .map_or(false, |config| config.fan_curve.enabled);
        if !enabled {
            return;
        }
        let target = match self.fan_curve_last_rpm {
            Some(rpm) => rpm,
            None => return, // never computed yet; the next curve tick establishes
        };
        if let Some(laptop) = self.get_device() {
            // set_fan_manual's per-command settle (200ms after each 0x0d/0x02
            // write) latches manual mode before the speed writes; mirrors
            // fan_curve_tick.
            laptop.set_fan_manual();
            laptop.set_zone_rpm(0x01, target);
            laptop.set_zone_rpm(0x02, target);
        }
        self.fan_curve_established = true;
    }

    pub fn set_logo_led_state(&mut self, ac:usize, logo_state: u8) -> bool {
        let mut res: bool = false;
        let mut is_synced = false;
        
        if let Some(config) = self.get_config() {
            is_synced = config.sync;
            config.power[ac].logo_state = logo_state;
            if config.sync {
                let other = (ac + 1) & 0x01;
                config.power[other].logo_state = logo_state;
            }
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }
             
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
           
            if state != ac && !is_synced {
                res = true;
            } else {
                res = laptop.set_logo_led_state(logo_state);
            }
        }

        return res;
    }

    pub fn get_logo_led_state(&mut self, ac: usize) -> u8 {
        // if let Some(laptop) = self.get_device() {
            // if laptop.ac_state as usize == ac {
                // return laptop.get_logo_led_state();
            // }
        // }
    
        if let Some(config) = self.get_ac_config(ac) {
            return config.logo_state;
        }

        return 0;
    }

    pub fn set_brightness(&mut self, ac:usize, brightness: u8) -> bool {
        let mut res: bool = false;
        let clamped = if brightness > 100 { 100u16 } else { brightness as u16 };
        let _val = clamped * 255 / 100;
        let mut is_synced = false;
        
        if let Some(config) = self.get_config() {
            is_synced = config.sync;
            config.power[ac].brightness = _val as u8;
            if config.sync {
                let other = (ac + 1) & 0x01;
                config.power[other].brightness = _val as u8;
            }
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }
 
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            // If sync is enabled, the new brightness applies to both states, so update hardware regardless
            if state != ac && !is_synced {
                res = true;
            } else {
                res = laptop.set_brightness(_val as u8);
            }
        }

        return res;
    }

    pub fn get_brightness(&mut self, ac: usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            let val = config.brightness as u32;
            let mut perc = val * 100 * 100/ 255;
            perc += 50;
            perc /= 100;
            return perc as u8;
        }

        return 0
    }

    pub fn get_actual_fan_rpm(&mut self) -> i32 {
        if let Some(laptop) = self.get_device() {
            return laptop.read_fan_rpm_from_ec() as i32;
        }
        return 0;
    }

    pub fn get_fan_rpm(&mut self, ac: usize) -> i32 {
        let live_fan_setting = {
            if let Some(laptop) = self.get_device() {
                let state = laptop.get_ac_state();
                if state == ac {
                    laptop.read_fan_setting().map(|rpm| rpm as i32)
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(rpm) = live_fan_setting {
            return rpm;
        }

        if let Some(config) = self.get_ac_config(ac) {
            return config.fan_rpm;
        }

        return 0;
    }

    pub fn get_power_mode(&mut self, ac:usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            return config.power_mode;
        }

        return 0;
    }

    pub fn get_cpu_boost(&mut self, ac:usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            return config.cpu_boost;
        }

        return 0;
    }

    pub fn get_gpu_boost(&mut self, ac:usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            return config.gpu_boost;
        }

        return 0;
    }

    pub fn set_ac_state(&mut self, ac: bool) {
        // The EC may have a different fan state for the new AC profile; force the
        // curve task to re-assert manual mode on its next tick.
        self.fan_curve_established = false;
        if let Some(laptop) = self.get_device() {
            laptop.set_ac_state(ac);
        }
        self.change_idle = true;
        let config: Option<config::PowerConfig> = self.get_ac_config(ac as usize);
        if let Some(config) = config {
            if let Some(laptop) = self.get_device() {
                laptop.set_config(config);
            }
        }
        self.reassert_fan_curve();
    }

    pub fn set_ac_state_get(&mut self) {
        // Called on resume (and AC re-reads): the firmware resets fan state on
        // wake, so re-assert manual mode on the next curve tick.
        self.fan_curve_established = false;
        let dbus_system = match Connection::new_system() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Failed to connect to D-Bus system bus: {}", e);
                return;
            }
        };
        let proxy_ac = dbus_system.with_proxy("org.freedesktop.UPower", "/org/freedesktop/UPower/devices/line_power_AC0", time::Duration::from_millis(5000));
        use battery::OrgFreedesktopUPowerDevice;
        if let Ok(online) = proxy_ac.online() {
            if let Some(laptop) = self.get_device() {
                laptop.set_ac_state(online);
            }
            self.change_idle = true;
            let config: Option<config::PowerConfig> = self.get_ac_config(online as usize);
            if let Some(config) = config {
                if let Some(laptop) = self.get_device() {
                    laptop.set_config(config);
                }
            }
            self.reassert_fan_curve();
        }

    }

    pub fn get_device(&mut self) -> Option<&mut RazerLaptop> {
        return self.device.as_mut();
    }

    pub fn set_bho_handler(&mut self, is_on: bool, threshold: u8) -> bool {
        let result = self.get_device()
            .map_or(false, |laptop| laptop.set_bho(is_on, threshold));
        if result {
            if let Some(config) = self.get_config() {
                config.bho_on = is_on;
                config.bho_threshold = threshold;
                if let Err(e) = config.write_to_file() {
                    eprintln!("Error write config {:?}", e);
                }
            }
        }
        return result;
    }

    pub fn get_bho_handler(&mut self) -> Option<(bool, u8)> {
        // Check if device supports BHO
        let has_bho = self.get_device()
            .map_or(false, |laptop| laptop.have_feature("bho".to_string()));
        if !has_bho {
            return None;
        }
        if let Some(config) = self.get_config() {
            return Some((config.bho_on, config.bho_threshold));
        }
        return None;
    }

    pub fn restore_bho(&mut self) {
        let (bho_on, bho_threshold) = {
            match self.get_config() {
                Some(config) => (config.bho_on, config.bho_threshold),
                None => return,
            }
        };
        if bho_on {
            if let Some(laptop) = self.get_device() {
                laptop.set_bho(bho_on, bho_threshold);
            }
        }
    }

    fn get_config(&mut  self) -> Option<&mut config::Configuration> {
        return self.config.as_mut();
    }

    // pub fn set_device(&mut self, device: RazerLaptop) {
        // self.device = Some(device);
    // }

    pub fn find_supported_device(&mut self, vid: u16, pid: u16) -> Option<&SupportedDevice> {
        for device in &self.supported_devices {
            // Unwrap: we control the strings and know they are are valid
            let svid = u16::from_str_radix(&device.vid, 16).unwrap();
            let spid = u16::from_str_radix(&device.pid, 16).unwrap();

            if svid == vid && spid == pid {
                return Some(device);
            }
        }

        None
    }

    pub fn discover_devices(&mut self)  {
        // Check if socket is OK
        match HidApi::new() {
            Ok(api) => {
                // Primary path: interface 0 via hidapi.
                // hidapi's linux-native (hidraw) backend returns -1 for
                // interface_number(), so resolve the real USB interface
                // number from sysfs when the value is unavailable.
                for device in api.device_list().filter(|d| d.vendor_id() == RAZER_VENDOR_ID) {
                    let iface = if device.interface_number() >= 0 {
                        device.interface_number()
                    } else {
                        // Derive interface from sysfs via the device path
                        let path_str = device.path().to_str().unwrap_or_default();
                        let hidraw_name = path_str.rsplit('/').next().unwrap_or("");
                        Self::hidraw_iface_number(hidraw_name).unwrap_or(-1)
                    };
                    if iface != 0 {
                        continue;
                    }

                    if let Some(supported_device) = self.find_supported_device(device.vendor_id(), device.product_id()) {
                        match api.open_path(device.path()) {
                            Ok(dev) => {
                                self.device = Some(RazerLaptop::new(
                                    supported_device.name.clone(),
                                    supported_device.features.clone(),
                                    supported_device.fan.clone(),
                                    dev,
                                ));
                                return;
                            }
                            Err(e) => {
                                eprintln!(
                                    "Failed to open supported device on iface 0 ({:04x}:{:04x}): {}",
                                    device.vendor_id(),
                                    device.product_id(),
                                    e
                                );
                            }
                        }
                    }
                }

                // Fallback #1: direct /dev/hidrawX probing based on /sys VID/PID.
                // Collect candidates and sort by USB interface number so we
                // prefer interface 0 (the one that accepts feature reports).
                let mut candidates: Vec<(String, u16, u16, i32)> = Vec::new();
                if let Ok(entries) = fs::read_dir("/dev") {
                    for entry in entries.flatten() {
                        let name = match entry.file_name().into_string() {
                            Ok(n) => n,
                            Err(_) => continue,
                        };
                        if !name.starts_with("hidraw") {
                            continue;
                        }

                        let Some((vid, pid)) = Self::hidraw_vid_pid(&name) else {
                            continue;
                        };

                        if vid != RAZER_VENDOR_ID {
                            continue;
                        }

                        let iface = Self::hidraw_iface_number(&name).unwrap_or(999);
                        eprintln!("hidraw fallback candidate: /dev/{} vid={:04x} pid={:04x} iface={}", name, vid, pid, iface);
                        candidates.push((name, vid, pid, iface));
                    }
                }
                candidates.sort_by_key(|c| c.3); // prefer lowest interface number

                for (name, vid, pid, iface) in candidates {
                    if let Some(supported_device) = self.find_supported_device(vid, pid) {
                        let path = format!("/dev/{}", name);
                        let c_path = match CString::new(path.clone()) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        eprintln!(
                            "Trying hidraw fallback open for {} ({:04x}:{:04x}) on {} (iface {})",
                            supported_device.name,
                            vid,
                            pid,
                            path,
                            iface,
                        );
                        match api.open_path(c_path.as_c_str()) {
                            Ok(dev) => {
                                self.device = Some(RazerLaptop::new(
                                    supported_device.name.clone(),
                                    supported_device.features.clone(),
                                    supported_device.fan.clone(),
                                    dev,
                                ));
                                return;
                            }
                            Err(e) => {
                                eprintln!(
                                    "hidraw fallback open failed for {} ({:04x}:{:04x}) on {}: {}",
                                    supported_device.name,
                                    vid,
                                    pid,
                                    path,
                                    e
                                );
                            }
                        }
                    }
                }

                eprintln!("No supported Razer HID device could be opened");
            },
            Err(e) => {
                eprintln!("Error: {}", e);
            },
        }
    }
}

pub struct RazerLaptop {
    name: String,
    pub(crate) features: Vec<String>,
    fan: Vec<u16>,
    device: hidapi::HidDevice,
    power: u8, // need for fan
    fan_rpm: u8, // need for power
    ac_state: u8, // index config array
    screensaver: bool,
    transaction_id: u8,
}
//
impl RazerLaptop {
// LED STORAGE Options
    const NOSTORE:u8 = 0x00;
    const VARSTORE:u8 = 0x01;
// LED definitions
    const LOGO_LED:u8 = 0x04;
    const BACKLIGHT_LED:u8 = 0x05;
// effects
    pub const OFF:u8 = 0x00;
    pub const WAVE:u8 = 0x01;
    pub const REACTIVE:u8 = 0x02; // Afterglo
    #[allow(dead_code)]
    pub const BREATHING:u8 = 0x03;
    pub const SPECTRUM:u8 = 0x04;
    pub const CUSTOMFRAME:u8 = 0x05;
    pub const STATIC:u8 = 0x06;
    #[allow(dead_code)]
    pub const STARLIGHT:u8 = 0x19;

    // Command-confirm tuning, mirroring Synapse's UsbRzDeviceAction handshake:
    // write the report, then re-read the reply until the EC reports SUCCESS. The
    // EC answers BUSY/NEW while it is still processing (notably right after
    // resume) and leaves the previous command's reply in the buffer when read too
    // early. The old path read once after a flat 1ms, so unconfirmed writes slipped
    // through and the GPU/fan re-apply bursts had to paper over them.
    const SEND_WRITE_ATTEMPTS: usize = 3;
    const SEND_READ_POLLS: usize = 20;
    const SEND_POLL_INTERVAL_MS: u64 = 5;

    pub fn new(name: String, features: Vec<String>, fan: Vec<u16>, device: hidapi::HidDevice) -> RazerLaptop {
        return RazerLaptop{
            name,
            features,
            fan,
            device,
            power: 0,
            fan_rpm: 0,
            ac_state: 0,
            screensaver: false,
            transaction_id: 0,
        };
    }

    pub fn set_screensaver(&mut self, active: bool) {
        self.screensaver = active;
    }

    pub fn set_config(&mut self, config: config::PowerConfig) -> bool {
        let mut ret: bool = false;

        if !self.screensaver {
            ret |= self.set_brightness(config.brightness);
            ret |= self.set_logo_led_state(config.logo_state);
        } else {
            ret |= self.set_brightness(0);
            ret |= self.set_logo_led_state(0);
        }
        ret |= self.set_power_mode(config.power_mode, config.cpu_boost, config.gpu_boost);
        // When a smart curve owns the fans, leave the speed to the curve task so
        // an AC/profile switch doesn't briefly drop the fans to auto/fixed.
        if !config.fan_curve.enabled {
            ret |= self.set_fan_rpm(config.fan_rpm as u16);
        }

        return ret;
    }

    pub fn set_ac_state(&mut self, online: bool) -> usize {
        if online {
            self.ac_state = 1;
        } else {
            self.ac_state = 0;
        }

        return  self.ac_state as usize;
    }

    pub fn get_ac_state(&mut self) -> usize {
        return self.ac_state as usize;
    }

    pub fn get_name(&self) -> String {
        return self.name.clone();
    }

    pub fn have_feature(&mut self, fch: String) -> bool {
        return self.features.contains(&fch);
    }

    fn clamp_fan(&mut self, rpm: u16) -> u8 {
        if rpm > self.fan[1] {
            return (self.fan[1] / 100) as u8;
        }
        if rpm < self.fan[0] {
            return (self.fan[0] / 100) as u8;
        }

        return (rpm / 100) as u8;
    }

    fn clamp_u8(&mut self, value: u8, min: u8, max: u8) ->u8 {
        if value > max {
            return max;
        }
        if value < min {
            return min;
        }

        return value;
    }

    pub fn set_standard_effect(&mut self, effect_id: u8, params: Vec<u8>) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x0a, 80);
        report.args[0] = effect_id; // effect id
        if !params.is_empty() {
            let len = params.len().min(79); // args[0] is effect_id, so max 79 param bytes
            for idx in 0..len {
                report.args[idx+1] = params[idx];
            }
        }
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    pub fn set_custom_frame_data(&mut self, row: u8, data: Vec<u8>) {
        // if data.len() == kbd::board::KEYS_PER_ROW {
        if data.len() == 45 {
            let mut report: RazerPacket = RazerPacket::new(0x03, 0x0b, 0x34);
            report.args[0] = 0xff;
            report.args[1] = row;
            report.args[2] = 0x00; // start col
            report.args[3] = 0x0f; // end col
            for idx in 0..data.len() {
                report.args[idx + 7] = data[idx];
            }
            self.send_report(report);
        }
    }

    pub fn set_custom_frame(&mut self) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x0a, 0x02);
        report.args[0] = RazerLaptop::CUSTOMFRAME; // effect id
        report.args[1] = RazerLaptop::NOSTORE;
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    // Retained as an EC diagnostic read (0x0d/0x87 boost readback / 0x0d/0x82
    // zone state) for measurement sessions — e.g. probing what the EC reports
    // after Synapse's undervolt toggle. No production caller since v2.7 removed
    // the lineage-inherited throwaway reads from the Custom-entry choreography.
    #[allow(dead_code)]
    pub fn get_power_mode(&mut self, zone: u8) -> u8 {
        if let Some((mode_byte, _manual_flag)) = self.read_zone_fan_state(zone) {
            return mode_byte;
        }
        return 0;
    }

    fn read_zone_fan_state(&mut self, zone: u8) -> Option<(u8, u8)> {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x82, 0x04);
        // profileId=1 must match the write paths so readback queries the same slot.
        report.args[0] = 0x01;
        report.args[1] = zone;
        report.args[2] = 0x00;
        report.args[3] = 0x00;
        self.send_report(report)
            .map(|response| (response.args[2], response.args[3]))
    }

    /// Set a fan zone's mode via Set Thermal Fan Mode (0x0d/0x02).
    /// Wire layout matches Synapse: [profileId=1, fanId, fanMode, fanModeValue].
    /// `fanMode` (args[2]) MUST be the currently-active performance mode: the EC
    /// keys the per-zone manual/auto setting to that mode's slot, so a constant
    /// here writes the setting to an inactive slot. Using `self.power` keeps the
    /// write on the same slot `set_power()` activates.
    /// `manual_flag` (args[3]): 1 = manual, 0 = auto.
    fn set_zone_fan_state(&mut self, zone: u8, manual_flag: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x02, 0x04);
        report.args[0] = 0x01;
        report.args[1] = zone;
        report.args[2] = self.power;
        report.args[3] = manual_flag;
        self.send_report(report).is_some()
    }

    fn read_stored_fan_setpoint(&mut self, zone: u8) -> Option<u16> {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x81, 0x03);
        // profileId=1 must match the write paths so readback queries the same slot.
        report.args[0] = 0x01;
        report.args[1] = zone;
        report.args[2] = 0x00;
        self.send_report(report)
            .map(|response| response.args[2] as u16 * 100)
    }

    pub fn read_fan_setting(&mut self) -> Option<u16> {
        let (_mode_byte, manual_flag) = self.read_zone_fan_state(0x01)?;
        if manual_flag == 0 {
            return Some(0);
        }
        self.read_stored_fan_setpoint(0x01)
    }

    fn set_power(&mut self, zone: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x02, 0x04);
        // profileId=1 must match set_zone_fan_state so byte0 does not thrash 0<->1.
        report.args[0] = 0x01;
        report.args[1] = zone;
        report.args[2] = self.power;
        match self.fan_rpm {
            0 => report.args[3] = 0x00,
            _ => report.args[3] = 0x01
        }
        if let Some(_) = self.send_report(report) {
            return  true;
        }

        return false;
    }

    // Retained as an EC diagnostic read (0x0d/0x87 boost readback / 0x0d/0x82
    // zone state) for measurement sessions — e.g. probing what the EC reports
    // after Synapse's undervolt toggle. No production caller since v2.7 removed
    // the lineage-inherited throwaway reads from the Custom-entry choreography.
    #[allow(dead_code)]
    pub fn get_cpu_boost(&mut self) -> u8 {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x87, 0x03);
        report.args[0] = 0x01;
        report.args[1] = 0x01;
        report.args[2] = 0x00;
        if let Some(response) = self.send_report(report) {
            return response.args[2];
        }
        return 0;
    }

    fn set_cpu_boost(&mut self, mut boost: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x07, 0x03);
        if boost == 3 && !self.have_feature("boost".to_string()) {
            boost = 2;
        }
        report.args[0] = 0x01;
        report.args[1] = 0x01;
        report.args[2] = boost;
        if let Some(_)= self.send_report(report) {
            return true;
        }

        return false;
    }

    // Retained as an EC diagnostic read (0x0d/0x87 boost readback / 0x0d/0x82
    // zone state) for measurement sessions — e.g. probing what the EC reports
    // after Synapse's undervolt toggle. No production caller since v2.7 removed
    // the lineage-inherited throwaway reads from the Custom-entry choreography.
    #[allow(dead_code)]
    fn get_gpu_boost(&mut self) -> u8 {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x87, 0x03);
        report.args[0] = 0x01;
        report.args[1] = 0x02;
        report.args[2] = 0x00;
        if let Some(response) = self.send_report(report){
            return response.args[2];
        }
        return 0;
    }

    fn set_gpu_boost(&mut self, boost: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x07, 0x03);
        report.args[0] = 0x01;
        report.args[1] = 0x02;
        report.args[2] = boost;
        if let Some(_) = self.send_report(report) {
            return true;
        }
        return false;
    }

    pub fn set_power_mode(&mut self, mode: u8, cpu_boost: u8, gpu_boost: u8) -> bool {
        // Wire value is written verbatim (args[2] in set_power). Domain validity
        // and the exposed profile set are enforced upstream in the CLI; the EC
        // itself accepts any in-range value (verified: it does not enforce the
        // AC/DC domain, only rejects values > 7). Blade 16 2025 measured map:
        //   0 Balanced(AC)  2 Performance  3 BatterySaver(DC)  4 Custom
        //   5 Silent        6 Balanced(DC)
        // (1 = legacy Gaming, 7 = HyperBoost/cooling-pad — never sent by this tool.)
        // Safety guard (design: never emit value 7). Wire value 7 is the
        // cooling-pad "HyperBoost" 175 W envelope; running it without the pad's
        // airflow is out of scope. This is the single chokepoint every caller
        // (CLI, GUI, config restore, AC-switch reapply) funnels through, so the
        // EC can never receive 7 regardless of how the request originated.
        if mode == 7 {
            eprintln!("refusing to set profile 7 (cooling-pad HyperBoost) — unsafe without the pad");
            return false;
        }
        self.power = mode;
        if mode == 4 {
            // Custom: mirror Synapse's MEASURED choreography (AC/DC captures
            // 2026-07): exactly four writes, NO reads — zone 1 profile, zone 2
            // profile, then CPU boost, then GPU boost. The former
            // read-before-write pattern was lineage-inherited: those captures
            // show Synapse writing straight through, and the read results were
            // discarded here anyway (never fed any logic). Boosts now land
            // after BOTH zones sit in the Custom slot, matching the wire.
            self.fan_rpm = 0;
            self.set_power(0x01);
            self.set_power(0x02);
            self.set_cpu_boost(cpu_boost);
            self.set_gpu_boost(gpu_boost);
        } else {
            self.set_power(0x01);
            self.set_power(0x02);
        }

        return true;
    }

    fn set_rpm(&mut self, zone: u8) -> bool {
        let mut report:RazerPacket = RazerPacket::new(0x0d, 0x01, 0x03);
        // Set fan RPM. profileId=1 matches Synapse's classId (Set Thermal Fan Speed).
        report.args[0] = 0x01;
        report.args[1] = zone;
        report.args[2] = self.fan_rpm;
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    pub fn set_fan_rpm(&mut self, value: u16) -> bool {
        if value == 0 {
            self.fan_rpm = 0;
            let zone1 = self.set_zone_fan_state(0x01, 0x00);
            let zone2 = self.set_zone_fan_state(0x02, 0x00);
            return zone1 && zone2;
        }

        self.fan_rpm = self.clamp_fan(value);
        let zone1 = self.set_zone_fan_state(0x01, 0x01);
        let zone2 = self.set_zone_fan_state(0x02, 0x01);
        let fan1 = self.set_rpm(0x01);
        let fan2 = self.set_rpm(0x02);

        return zone1 && zone2 && fan1 && fan2;
    }

    #[allow(dead_code)]
    pub fn get_fan_rpm(&mut self) -> u16 {
        let res: u16 = self.fan_rpm as u16;
        return res * 100;
    }

    /// Latch both fan zones into manual mode (fanModeValue=1) without setting a
    /// speed. Used by the curve task before its first speed write after a transition.
    pub fn set_fan_manual(&mut self) -> bool {
        let zone1 = self.set_zone_fan_state(0x01, 0x01);
        let zone2 = self.set_zone_fan_state(0x02, 0x01);
        return zone1 && zone2;
    }

    /// Set a single fan zone's RPM (clamped to the model range). Assumes the
    /// zone is already in manual mode.
    pub fn set_zone_rpm(&mut self, zone: u8, rpm: u16) -> bool {
        self.fan_rpm = self.clamp_fan(rpm);
        return self.set_rpm(zone);
    }

    /// Read fan RPM from EC hardware.
    /// Note: on many Razer models this returns the configured target,
    /// not measured tachometer RPM (no tach register exposed via USB HID).
    pub fn read_fan_rpm_from_ec(&mut self) -> u16 {
        if let Some(rpm) = self.read_stored_fan_setpoint(0x01) {
            return rpm;
        }
        return self.fan_rpm as u16 * 100;
    }

    pub fn set_logo_led_state(&mut self, mode: u8) -> bool {
        if mode > 0 {
            let mut report: RazerPacket = RazerPacket::new(0x03, 0x02, 0x03);
            report.args[0] = RazerLaptop::VARSTORE;
            report.args[1] = RazerLaptop::LOGO_LED;
            if mode == 1 {
                report.args[2] = 0x00;
            } else if mode == 2 {
                report.args[2] = 0x02;
            }
            self.send_report(report);
        }

        let mut report: RazerPacket = RazerPacket::new(0x03, 0x00, 0x03);
        report.args[0] = RazerLaptop::VARSTORE;
        report.args[1] = RazerLaptop::LOGO_LED;
        report.args[2] = self.clamp_u8(mode, 0x00, 0x01);
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    #[allow(dead_code)]
    pub fn get_logo_led_state(&mut self) -> u8 {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x82, 0x03);
        report.args[0] = RazerLaptop::VARSTORE;
        report.args[1] = RazerLaptop::LOGO_LED;
        if let Some(response) = self.send_report(report){
            return response.args[2];
        }
        return 0;
    }

    pub fn set_brightness(&mut self, brightness: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x03, 0x03);
        report.args[0] = RazerLaptop::VARSTORE;
        report.args[1] = RazerLaptop::BACKLIGHT_LED;
        report.args[2] = brightness;
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    #[allow(dead_code)]
    pub fn get_brightness(&mut self) -> u8 {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x83, 0x03);
        report.args[0] = RazerLaptop::VARSTORE;
        report.args[1] = RazerLaptop::BACKLIGHT_LED;
        report.args[2] = 0x00;
        if let Some(response) = self.send_report(report){
            return response.args[2];
        }
        return 0;
    }

    #[allow(dead_code)]
    pub fn get_bho(&mut self) -> Option<u8> {
        if !self.have_feature("bho".to_string()) {
            return None;
        }

        let mut report: RazerPacket = RazerPacket::new(0x07, 0x92, 0x01);
        report.args[0] = 0x00;

        return self.send_report(report)
            .map(|resp| resp.args[0]);
    }

    pub fn set_bho(&mut self, is_on: bool, threshold: u8) -> bool {
        if !self.have_feature("bho".to_string()) {
            return false;
        }

        let mut report = RazerPacket::new(0x07, 0x12, 0x01);
        report.args[0] = bho_to_byte(is_on, threshold);

        return self.send_report(report)
            .map_or(false, |r| { 
                println!("Response Packet:\n{:#?}", r); 
                true
            } 
        );
    }

    fn next_transaction_id(&mut self) -> u8 {
        // Synapse's transaction id cycles 1..=30 (measured, AC/DC captures
        // 2026-07: across 35 frames the ids run 0x01..0x1e globally monotonic
        // and wrap 0x1e -> 0x01). Neither 0x00 nor 0x1f ever appears on the
        // wire, so this emits neither — never emit what Synapse would not.
        self.transaction_id += 1;
        if self.transaction_id > 30 {
            self.transaction_id = 1;
        }
        self.transaction_id
    }

    fn send_report(&mut self, mut report: RazerPacket) -> Option<RazerPacket> {
        let poll_interval = time::Duration::from_millis(Self::SEND_POLL_INTERVAL_MS);

        for _ in 0..Self::SEND_WRITE_ATTEMPTS {
            // Rotate the transaction id per write so a resend is not mistaken for a
            // duplicate of the previous attempt.
            report.id = self.next_transaction_id();
            let request = report.calc_crc();
            if let Err(e) = self.device.send_feature_report(request.as_slice()) {
                eprintln!("HID write failed: {}", e);
                thread::sleep(poll_interval);
                continue;
            }

            let mut resend = false;
            for poll in 0..Self::SEND_READ_POLLS {
                // Read immediately on the first poll: when the EC already has the
                // reply buffered, return without paying a full poll interval. A
                // not-yet-ready or stale reply still classifies as KeepPolling, so
                // later polls sleep and re-read exactly as before.
                if poll > 0 {
                    thread::sleep(poll_interval);
                }
                let mut buf: [u8; 91] = [0x00; 91];
                let size = match self.device.get_feature_report(&mut buf) {
                    Ok(size) => size,
                    Err(e) => {
                        eprintln!("HID read failed: {}", e);
                        continue;
                    }
                };
                if size != 91 {
                    continue;
                }
                let response = match bincode::deserialize::<RazerPacket>(&buf) {
                    Ok(response) => response,
                    Err(e) => {
                        eprintln!("Response decode failed: {}", e);
                        continue;
                    }
                };
                match classify_response(&report, &response) {
                    ResponseAction::Accept => {
                        // Let the EC finish latching a thermal change before the next
                        // command races it (Synapse's post-write J(200)/J(100)).
                        let settle = thermal_settle_ms(report.command_class, report.command_id);
                        if settle > 0 {
                            thread::sleep(time::Duration::from_millis(settle));
                        }
                        return Some(response);
                    }
                    ResponseAction::KeepPolling => continue,
                    ResponseAction::Resend => {
                        resend = true;
                        break;
                    }
                    ResponseAction::Unsupported => {
                        eprintln!(
                            "Command not supported (class {:#04x} id {:#04x})",
                            report.command_class, report.command_id
                        );
                        return None;
                    }
                }
            }

            if !resend {
                // Polls exhausted with the EC still busy: hammering it further rarely
                // helps once it has gone quiet, so stop instead of resending.
                break;
            }
        }

        None
    }

}

/// How `send_report` should react to a feature-report reply, mirroring Synapse's
/// `getCommandSendStatus`.
#[derive(PartialEq, Debug)]
enum ResponseAction {
    /// Reply matches the request and reports success: hand it back.
    Accept,
    /// EC is still processing (BUSY/NEW/TIMEOUT) or the buffer still holds a
    /// previous command's reply: read again without resending.
    KeepPolling,
    /// EC reported an explicit failure: write the command again.
    Resend,
    /// EC does not support the command: give up, resending will not help.
    Unsupported,
}

/// Classify a feature-report reply against the request that was just written.
/// Pure decision logic, separated from the HID I/O so it can be unit-tested.
fn classify_response(request: &RazerPacket, response: &RazerPacket) -> ResponseAction {
    // Battery-health-optimizer replies come back with command id 0x92 whether the
    // request was the get (0x92) or the set (0x12); accept those for BHO requests
    // only, so a stale BHO reply is never taken as another command's response.
    if response.command_id == 0x92 && (request.command_id == 0x92 || request.command_id == 0x12) {
        log_tid_mismatch(request, response);
        return ResponseAction::Accept;
    }
    if response.command_class != request.command_class
        || response.command_id != request.command_id
        || response.remaining_packets != request.remaining_packets
    {
        return ResponseAction::KeepPolling;
    }
    match response.status {
        RazerPacket::RAZER_CMD_SUCCESSFUL => {
            // TID staleness guard, PROMOTED from the log-only stage: the EC
            // echoes the request's transaction id — verified live (thousands
            // of accepted replies in a single day, zero `TID mismatch` journal
            // lines; a non-echoing EC would have logged on every accept). A
            // matching class/id/remaining reply carrying the WRONG id is
            // therefore the previous command's buffered reply — exactly the
            // back-to-back zone1/zone2 race — so poll on for ours instead of
            // accepting it.
            if response.id != request.id {
                eprintln!(
                    "Stale reply (TID {:#04x}, expected {:#04x}) for class {:#04x} id {:#04x} — polling on",
                    response.id, request.id, response.command_class, response.command_id
                );
                return ResponseAction::KeepPolling;
            }
            ResponseAction::Accept
        }
        RazerPacket::RAZER_CMD_BUSY
        | RazerPacket::RAZER_CMD_NEW
        | RazerPacket::RAZER_CMD_TIMEOUT => ResponseAction::KeepPolling,
        RazerPacket::RAZER_CMD_NOT_SUPPORTED => ResponseAction::Unsupported,
        RazerPacket::RAZER_CMD_FAILURE => ResponseAction::Resend,
        // Any out-of-spec status: resend defensively rather than trust the reply.
        _ => ResponseAction::Resend,
    }
}

/// TID-echo diagnostic, LOG-ONLY — now applies to the BHO special path alone.
/// The main path's echo behaviour is proven (see the promoted guard above);
/// BHO replies are the documented oddball (command id 0x92 for get AND set),
/// and the restore-at-boot sample is too small to enforce on. A few deliberate
/// BHO toggles with a silent journal promote this path too.
fn log_tid_mismatch(request: &RazerPacket, response: &RazerPacket) {
    if response.id != request.id {
        eprintln!(
            "TID mismatch on accepted reply: sent {:#04x}, got {:#04x} (class {:#04x} id {:#04x})",
            request.id, response.id, response.command_class, response.command_id
        );
    }
}

/// Settle delay Synapse waits after a thermal write before the next command, so
/// the EC finishes latching one change first: 200ms after Set Thermal Fan Mode
/// (0x0d/0x02), 100ms after Set Thermal Fan Speed (0x0d/0x01) and the boost write
/// (0x0d/0x07). Reads and non-thermal commands do not settle.
fn thermal_settle_ms(command_class: u8, command_id: u8) -> u64 {
    match (command_class, command_id) {
        (0x0d, 0x02) => 200,
        (0x0d, 0x01) | (0x0d, 0x07) => 100,
        _ => 0,
    }
}

/// Step/ceiling lookup: returns the RPM of the lowest curve point whose
/// `temp_c` is still strictly greater than `temp`. Above the highest point the
/// daemon clamps to the top point's RPM (Synapse stops updating there, which is
/// unsafe for sustained load). Points must be sorted by `temp_c` ascending.
fn lookup_rpm(points: &[FanCurvePoint], temp: f64) -> Option<u16> {
    let last = points.last()?;
    for point in points {
        if f64::from(point.temp_c) > temp {
            return Some(point.rpm);
        }
    }
    Some(last.rpm)
}

/// Resolve a single target RPM for both fan zones from a curve and the available
/// temperatures. For `Both`, each temp is looked up on its own curve and the
/// higher resulting RPM wins (NOT the higher temperature).
fn compute_curve_rpm(curve: &FanCurve, cpu_temp: Option<f64>, gpu_temp: Option<f64>) -> Option<u16> {
    let cpu_rpm = cpu_temp.and_then(|t| lookup_rpm(&curve.cpu_points, t));
    let gpu_rpm = gpu_temp.and_then(|t| lookup_rpm(&curve.gpu_points, t));
    match curve.source {
        CurveTempSource::Cpu => cpu_rpm,
        CurveTempSource::Gpu => gpu_rpm,
        CurveTempSource::Both => match (cpu_rpm, gpu_rpm) {
            (Some(c), Some(g)) => Some(c.max(g)),
            (Some(c), None) => Some(c),
            (None, Some(g)) => Some(g),
            (None, None) => None,
        },
    }
}

// top bit flags whether battery health optimization is on or off
// bottom bits are the actual threshold that it is set to
#[allow(dead_code)]
fn byte_to_bho(u: u8) -> (bool, u8) {
    return (u & (1 << 7) != 0, (u & 0b0111_1111));
}

fn bho_to_byte(is_on: bool, threshold: u8) -> u8 {
    if is_on {
        return threshold | 0b1000_0000;
    }
    return threshold;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(command_class: u8, command_id: u8, status: u8) -> RazerPacket {
        let mut packet = RazerPacket::new(command_class, command_id, 0x00);
        packet.status = status;
        packet
    }

    #[test]
    fn accepts_matching_success() {
        let request = RazerPacket::new(0x0d, 0x02, 0x04);
        let response = reply(0x0d, 0x02, RazerPacket::RAZER_CMD_SUCCESSFUL);
        assert_eq!(classify_response(&request, &response), ResponseAction::Accept);
    }

    #[test]
    fn keeps_polling_while_busy() {
        let request = RazerPacket::new(0x0d, 0x02, 0x04);
        for status in [
            RazerPacket::RAZER_CMD_BUSY,
            RazerPacket::RAZER_CMD_NEW,
            RazerPacket::RAZER_CMD_TIMEOUT,
        ] {
            let response = reply(0x0d, 0x02, status);
            assert_eq!(
                classify_response(&request, &response),
                ResponseAction::KeepPolling
            );
        }
    }

    #[test]
    fn keeps_polling_on_stale_mismatched_reply() {
        // A leftover reply to a different command must not be accepted as ours.
        let request = RazerPacket::new(0x0d, 0x02, 0x04);
        let response = reply(0x0d, 0x01, RazerPacket::RAZER_CMD_SUCCESSFUL);
        assert_eq!(
            classify_response(&request, &response),
            ResponseAction::KeepPolling
        );
    }

    #[test]
    fn resends_on_failure() {
        let request = RazerPacket::new(0x0d, 0x02, 0x04);
        let response = reply(0x0d, 0x02, RazerPacket::RAZER_CMD_FAILURE);
        assert_eq!(classify_response(&request, &response), ResponseAction::Resend);
    }

    #[test]
    fn stale_tid_reply_keeps_polling() {
        // Same class/id/remaining and SUCCESS, but the echoed transaction id
        // belongs to the previous command: must be treated as a stale buffered
        // reply, not accepted as ours.
        let mut request = RazerPacket::new(0x0d, 0x02, 0x04);
        request.id = 0x05;
        let mut response = reply(0x0d, 0x02, RazerPacket::RAZER_CMD_SUCCESSFUL);
        response.id = 0x04;
        assert_eq!(
            classify_response(&request, &response),
            ResponseAction::KeepPolling
        );
    }

    #[test]
    fn unsupported_is_terminal() {
        let request = RazerPacket::new(0x0d, 0x02, 0x04);
        let response = reply(0x0d, 0x02, RazerPacket::RAZER_CMD_NOT_SUPPORTED);
        assert_eq!(
            classify_response(&request, &response),
            ResponseAction::Unsupported
        );
    }

    #[test]
    fn accepts_bho_reply_for_bho_request() {
        let request = RazerPacket::new(0x07, 0x92, 0x01);
        let response = reply(0x07, 0x92, RazerPacket::RAZER_CMD_SUCCESSFUL);
        assert_eq!(classify_response(&request, &response), ResponseAction::Accept);
    }

    #[test]
    fn ignores_stray_bho_reply_for_other_request() {
        let request = RazerPacket::new(0x0d, 0x02, 0x04);
        let mut response = reply(0x0d, 0x02, RazerPacket::RAZER_CMD_SUCCESSFUL);
        response.command_id = 0x92;
        assert_eq!(
            classify_response(&request, &response),
            ResponseAction::KeepPolling
        );
    }

    #[test]
    fn fan_mode_settles_longest() {
        assert_eq!(thermal_settle_ms(0x0d, 0x02), 200);
        assert_eq!(thermal_settle_ms(0x0d, 0x01), 100);
        assert_eq!(thermal_settle_ms(0x0d, 0x07), 100);
        assert_eq!(thermal_settle_ms(0x0d, 0x82), 0);
        assert_eq!(thermal_settle_ms(0x03, 0x00), 0);
    }
}

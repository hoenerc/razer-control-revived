// mod kbd;
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;
use std::{thread, time, io, fs};
use std::ffi::CString;
use hidapi::HidApi;
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
        RazerPacket {
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
        }
    }

    fn calc_crc(&mut self) -> Vec<u8>{
        let mut buf: Vec<u8> = bincode::serialize(self).unwrap();
        // Razer CRC = XOR of the 90-byte report's bytes [2..88). Index 0 of this struct
        // is the prepended HID report-id, so that range maps to buf[3..89]; the crc byte
        // itself sits at buf[89].
        let mut res: u8 = 0x00;
        for &b in &buf[3..89] {
            res ^= b;
        }
        self.crc = res;
        buf[89] = res;
        buf
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
        DeviceManager {
            device: None,
            supported_devices: vec![],
            config: None,
            fan_curve_established: false,
            fan_curve_last_rpm: None,
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

        None
    }

    pub fn light_off(&mut self) {
        if let Some(laptop) = self.get_device() {
            laptop.set_brightness(0);
            laptop.set_logo_led_state(0);
        }
    }

    pub fn restore_light(&mut self) {
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
            laptop.set_brightness(brightness);
            laptop.set_logo_led_state(logo_state);
        }
    }

    /// Static-only lighting model (v2.10): persist + apply the one colour.
    pub fn set_static_color(&mut self, rgb: [u8; 3]) -> bool {
        let mut saved = true;
        if let Some(config) = self.get_config() {
            config.static_color = rgb;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
                saved = false;
            }
        }
        let mut applied = false;
        if let Some(laptop) = self.get_device() {
            applied = laptop.set_standard_effect(RazerLaptop::STATIC, rgb.to_vec());
        }
        applied && saved
    }

    pub fn get_static_color(&mut self) -> [u8; 3] {
        self.get_config().map(|c| c.static_color).unwrap_or([0, 255, 0])
    }

    /// True when the colour is on the hardware (or there was nothing to do:
    /// gate off, no device). False = the EC write failed; callers say so.
    pub fn restore_static_color(&mut self) -> bool {
        if !self.get_static_lighting() {
            return true; // lighting scope disabled — stay silent, touch nothing
        }
        let rgb = self.get_static_color();
        if let Some(laptop) = self.get_device() {
            println!("restore static colour: {:?}", rgb);
            return laptop.set_standard_effect(RazerLaptop::STATIC, rgb.to_vec());
        }
        true
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
        if !self.reassert_fan_curve() {
            eprintln!("power profile re-applied but the fan-curve re-assert failed — the curve retries on its next tick");
        }
        result
    }

    pub fn set_power_mode(&mut self, ac: usize, pwr: u8, cpu: u8, gpu: u8) -> bool {
        let mut res: bool = false;
        // The power-mode command rewrites the per-zone fan-state, so re-assert
        // manual fan mode on the next curve tick.
        self.fan_curve_established = false;
        let mut saved = true;
        if let Some(config) = self.get_config() {
            config.power[ac].power_mode = pwr;
            config.power[ac].cpu_boost = cpu;
            config.power[ac].gpu_boost = gpu;
            if let Err(e) = config.write_to_file() {
                // A failed persist means restore-on-boot silently reverts —
                // that is a failure of the request, not a footnote.
                eprintln!("Error write config {:?}", e);
                saved = false;
            }
        }
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if state != ac {
                // Inactive domain: stored now, applied by the restore path on
                // the next domain switch — deferred by design (docs/CONTRACTS).
                res = true;
            } else {
                res = laptop.set_power_mode(pwr, cpu, gpu);
            }
        }

        if !self.reassert_fan_curve() {
            eprintln!("profile written but the fan-curve re-assert failed — the curve retries on its next tick");
        }
        res && saved
    }

    /// Persist the experimental unlock and mirror it onto the live device.
    pub fn set_experimental_profiles(&mut self, enabled: bool) -> bool {
        if let Some(cfg) = self.config.as_mut() {
            cfg.experimental_profiles = enabled;
            if let Some(dev) = self.device.as_mut() {
                dev.allow_experimental = enabled;
            }
            if let Err(e) = cfg.write_to_file() {
                eprintln!("Failed to save config: {}", e);
                return false;
            }
            return true;
        }
        false
    }

    /// Master lighting switch: persist + mirror onto the live device — the
    /// laptop-level primitives gate on it, so every caller (AC-switch
    /// set_config, suspend hooks, boot restore, GUI setters) obeys through
    /// one chokepoint. Enabling re-applies the stored static colour;
    /// disabling leaves the keyboard exactly as-is, since switching it off
    /// would itself be a lighting write.
    /// Master gate — transactional, unlike the colour setter: a state toggle
    /// that returns false must mean "nothing changed" (the GUI snaps its
    /// switch back on false), while a colour write may legitimately persist a
    /// value the hardware refused this instant (see set_static_color).
    pub fn set_static_lighting(&mut self, enabled: bool) -> bool {
        let previous = match self.config.as_ref() {
            Some(cfg) => cfg.static_lighting,
            None => return false,
        };
        if previous == enabled {
            return true; // no-op request; skip the redundant fsync
        }
        // Persist FIRST: a live gate the disk does not know silently reverts
        // on the next boot — runtime must never outrun persistence.
        if !self.write_static_lighting(enabled, previous) {
            return false;
        }
        if let Some(dev) = self.device.as_mut() {
            dev.static_lighting = enabled;
        }
        if enabled && !self.restore_static_color() {
            // Colour re-apply failed: roll the gate back so disk, runtime and
            // hardware agree again and `false` keeps meaning "nothing changed".
            if let Some(dev) = self.device.as_mut() {
                dev.static_lighting = previous;
            }
            if !self.write_static_lighting(previous, previous) {
                eprintln!(
                    "static-lighting rollback could not be saved — disk keeps the requested value, runtime stays reverted until the next successful save"
                );
            }
            return false;
        }
        true
    }

    /// Write the gate value; on save failure the in-memory value reverts to
    /// `revert_to` so memory never claims a state the disk refused.
    fn write_static_lighting(&mut self, value: bool, revert_to: bool) -> bool {
        if let Some(cfg) = self.config.as_mut() {
            cfg.static_lighting = value;
            match cfg.write_to_file() {
                Ok(_) => true,
                Err(e) => {
                    eprintln!("Failed to save config: {}", e);
                    cfg.static_lighting = revert_to;
                    false
                }
            }
        } else {
            false
        }
    }

    pub fn get_static_lighting(&mut self) -> bool {
        self.get_config().map(|c| c.static_lighting).unwrap_or(true)
    }

    pub fn set_fan_rpm(&mut self, ac:usize, rpm: i32) -> bool {
        // Validate at the daemon boundary: 0 = auto, anything else must be a
        // plausible RPM inside the model's range. The old path cast i32 -> u16
        // unchecked, so -1 wrapped to 65535 (clamped to MAX fans) and 70000
        // wrapped to a silently wrong speed. Reject instead of reinterpret.
        let range_ok = self
            .device
            .as_ref()
            .is_some_and(|d| rpm >= d.fan[0] as i32 && rpm <= d.fan[1] as i32);
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

        res
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
        true
    }

    pub fn get_fan_curve(&mut self, ac: usize) -> FanCurve {
        if let Some(config) = self.get_ac_config(ac) {
            return config.fan_curve;
        }
        FanCurve::new()
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
            // Every write executes; the results combine afterwards.
            let manual_ok = if need_establish {
                // set_fan_manual issues two 0x0d/0x02 writes; send_report already
                // settles 200ms after each, latching manual mode before the speed
                // writes below. No additional sleep needed here.
                laptop.set_fan_manual()
            } else {
                true
            };
            let zone1_ok = laptop.set_zone_rpm(0x01, target);
            let zone2_ok = laptop.set_zone_rpm(0x02, target);
            if manual_ok && zone1_ok && zone2_ok {
                self.fan_curve_established = true;
                self.fan_curve_last_rpm = Some(target);
            } else {
                // A failed establish stays retryable: the next tick re-runs the
                // FULL sequence, and last_rpm must not record a target the EC
                // never confirmed.
                self.fan_curve_established = false;
                self.fan_curve_last_rpm = None;
            }
        }
    }

    /// Re-latch an active smart fan curve immediately after a code path rewrote
    /// the EC fan-state command (power-mode reapply, AC/profile switch, resume).
    /// Without this the curve only re-asserts on its next periodic tick (up to
    /// FAN_CURVE_POLL_SECS later); during the dGPU-resume reapply burst that
    /// deferred window is re-opened every ~2s, whipsawing the fans between the
    /// firmware auto curve and the manual target. Reuses the last computed target
    /// so no fresh temperature read is needed.
    /// Returns false when any re-assert write failed. On failure the curve is
    /// left un-established and the cached target is dropped, so the next curve
    /// tick re-runs the FULL sequence (manual latch + both zones) instead of
    /// early-outing on `established && last_rpm == target` — the same honesty
    /// rule fan_curve_tick follows. The no-work cases (no device, curve off,
    /// no target yet) return true: nothing failed.
    fn reassert_fan_curve(&mut self) -> bool {
        let ac = match self.get_device() {
            Some(laptop) => laptop.get_ac_state(),
            None => return true,
        };
        let enabled = self
            .get_ac_config(ac)
            .is_some_and(|config| config.fan_curve.enabled);
        if !enabled {
            return true;
        }
        let target = match self.fan_curve_last_rpm {
            Some(rpm) => rpm,
            None => return true, // never computed yet; the next curve tick establishes
        };
        let ok = match self.get_device() {
            Some(laptop) => {
                // set_fan_manual's per-command settle (200ms after each 0x0d/0x02
                // write) latches manual mode before the speed writes; mirrors
                // fan_curve_tick.
                let manual = laptop.set_fan_manual();
                let zone1 = laptop.set_zone_rpm(0x01, target);
                let zone2 = laptop.set_zone_rpm(0x02, target);
                manual && zone1 && zone2
            }
            None => true,
        };
        self.fan_curve_established = ok;
        if !ok {
            self.fan_curve_last_rpm = None;
        }
        ok
    }

    pub fn set_logo_led_state(&mut self, ac:usize, logo_state: u8) -> bool {
        let mut res: bool = false;
        
        if let Some(config) = self.get_config() {
            config.power[ac].logo_state = logo_state;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }
             
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
           
            if state != ac {
                res = true;
            } else {
                res = laptop.set_logo_led_state(logo_state);
            }
        }

        res
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

        0
    }

    pub fn set_brightness(&mut self, ac:usize, brightness: u8) -> bool {
        let mut res: bool = false;
        let clamped = if brightness > 100 { 100u16 } else { brightness as u16 };
        let _val = clamped * 255 / 100;
        
        if let Some(config) = self.get_config() {
            config.power[ac].brightness = _val as u8;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
            }
        }
 
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if state != ac {
                res = true;
            } else {
                res = laptop.set_brightness(_val as u8);
            }
        }

        res
    }

    pub fn get_brightness(&mut self, ac: usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            let val = config.brightness as u32;
            let mut perc = val * 100 * 100/ 255;
            perc += 50;
            perc /= 100;
            return perc as u8;
        }

        0
    }

    pub fn get_actual_fan_rpm(&mut self) -> i32 {
        if let Some(laptop) = self.get_device() {
            return laptop.read_fan_rpm_from_ec() as i32;
        }
        0
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

        0
    }

    pub fn get_power_mode(&mut self, ac:usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            return config.power_mode;
        }

        0
    }

    pub fn get_cpu_boost(&mut self, ac:usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            return config.cpu_boost;
        }

        0
    }

    pub fn get_gpu_boost(&mut self, ac:usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            return config.gpu_boost;
        }

        0
    }

    pub fn set_ac_state(&mut self, ac: bool) {
        // Mirror the experimental unlock onto the device (covers the first
        // apply after discovery; cheap on every domain pass).
        if let (Some(dev), Some(cfg)) = (self.device.as_mut(), self.config.as_ref()) {
            dev.allow_experimental = cfg.experimental_profiles;
            dev.static_lighting = cfg.static_lighting;
        }
        // The EC may have a different fan state for the new AC profile; force the
        // curve task to re-assert manual mode on its next tick.
        self.fan_curve_established = false;
        if let Some(laptop) = self.get_device() {
            laptop.set_ac_state(ac);
        }
        let config: Option<config::PowerConfig> = self.get_ac_config(ac as usize);
        if let Some(config) = config {
            if let Some(laptop) = self.get_device() {
                laptop.set_config(config);
            }
        }
        if !self.reassert_fan_curve() {
            eprintln!("AC/battery switch: fan-curve re-assert failed — the curve retries on its next tick");
        }
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
            let config: Option<config::PowerConfig> = self.get_ac_config(online as usize);
            if let Some(config) = config {
                if let Some(laptop) = self.get_device() {
                    laptop.set_config(config);
                }
            }
            if !self.reassert_fan_curve() {
                eprintln!("startup power sync: fan-curve re-assert failed — the curve retries on its next tick");
            }
        }

    }

    pub fn get_device(&mut self) -> Option<&mut RazerLaptop> {
        self.device.as_mut()
    }

    pub fn set_bho_handler(&mut self, is_on: bool, threshold: u8) -> bool {
        let result = self.get_device()
            .is_some_and(|laptop| laptop.set_bho(is_on, threshold));
        if result {
            if let Some(config) = self.get_config() {
                config.bho_on = is_on;
                config.bho_threshold = threshold;
                if let Err(e) = config.write_to_file() {
                    eprintln!("Error write config {:?}", e);
                }
            }
        }
        result
    }

    pub fn get_bho_handler(&mut self) -> Option<(bool, u8)> {
        // Check if device supports BHO
        let has_bho = self.get_device()
            .is_some_and(|laptop| laptop.have_feature("bho".to_string()));
        if !has_bho {
            return None;
        }
        if let Some(config) = self.get_config() {
            return Some((config.bho_on, config.bho_threshold));
        }
        None
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
        self.config.as_mut()
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
    transaction_id: u8,
    /// Config-backed experimental unlock; gates the HyperBoost chokepoint and
    /// the CPU/GPU boost-tier caps. Mirrored by the DeviceManager.
    pub(crate) allow_experimental: bool,
    pub(crate) static_lighting: bool,
}
//
impl RazerLaptop {
// LED STORAGE Options
    const VARSTORE:u8 = 0x01;
// LED definitions
    const LOGO_LED:u8 = 0x04;
    const BACKLIGHT_LED:u8 = 0x05;
// effects
    pub const STATIC:u8 = 0x06;

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
        RazerLaptop{
            name,
            features,
            fan,
            device,
            power: 0,
            fan_rpm: 0,
            ac_state: 0,
            transaction_id: 0,
            allow_experimental: false,
            static_lighting: true,
        }
    }

    pub fn set_config(&mut self, config: config::PowerConfig) -> bool {
        let mut ret: bool = false;

        ret |= self.set_brightness(config.brightness);
        ret |= self.set_logo_led_state(config.logo_state);
        ret |= self.set_power_mode(config.power_mode, config.cpu_boost, config.gpu_boost);
        // When a smart curve owns the fans, leave the speed to the curve task so
        // an AC/profile switch doesn't briefly drop the fans to auto/fixed.
        if !config.fan_curve.enabled {
            ret |= self.set_fan_rpm(config.fan_rpm as u16);
        }

        ret
    }

    pub fn set_ac_state(&mut self, online: bool) -> usize {
        if online {
            self.ac_state = 1;
        } else {
            self.ac_state = 0;
        }

        self.ac_state as usize
    }

    pub fn get_ac_state(&mut self) -> usize {
        self.ac_state as usize
    }

    pub fn get_name(&self) -> String {
        self.name.clone()
    }

    pub fn have_feature(&mut self, fch: String) -> bool {
        self.features.contains(&fch)
    }

    fn clamp_fan(&mut self, rpm: u16) -> u8 {
        clamp_fan_to_range(rpm, self.fan[0], self.fan[1])
    }

    fn clamp_u8(&mut self, value: u8, min: u8, max: u8) ->u8 {
        if value > max {
            return max;
        }
        if value < min {
            return min;
        }

        value
    }

    pub fn set_standard_effect(&mut self, effect_id: u8, params: Vec<u8>) -> bool {
        if !self.static_lighting {
            // Lighting scope disabled — the daemon never touches keyboard
            // lighting so external tools (OpenRazer, …) own it conflict-free.
            return true;
        }
        // Measured 2025 EC quirk (ec-protocol §23): the legacy 0x03/0x0a shim
        // applies ONE-BEHIND — it stores the incoming parameters and renders
        // the previously stored ones. Probe A (exact data_size instead of the
        // inherited 80) changed nothing; probe B (extended matrix 0x0f/0x02,
        // openrazer layout) rendered nothing either — the native 2025 command
        // is a capture question, not a guessing one. The measured remedy is
        // the operator's double-apply, promoted to code: send the confirmed
        // write twice. The second write flushes the first; its stored copy is
        // identical, so the shim pipeline stays consistent.
        let mut ok = false;
        for _ in 0..2 {
            let mut report: RazerPacket = RazerPacket::new(0x03, 0x0a, 1 + params.len().min(79) as u8);
            report.args[0] = effect_id; // effect id
            let len = params.len().min(79); // args[0] is effect_id, so max 79 param bytes
            report.args[1..=len].copy_from_slice(&params[..len]);
            ok = self.send_report(report).is_some();
            if !ok {
                break;
            }
        }
        ok
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
        0
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
        if self.send_report(report).is_some() {
            return  true;
        }

        false
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
        0
    }

    fn set_cpu_boost(&mut self, mut boost: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x07, 0x03);
        // Tier 3 is opt-in only — Synapse never sends it on this model.
        if boost == 3 && !self.allow_experimental {
            boost = 2;
        }
        report.args[0] = 0x01;
        report.args[1] = 0x01;
        report.args[2] = boost;
        if self.send_report(report).is_some() {
            return true;
        }

        false
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
        0
    }

    fn set_gpu_boost(&mut self, mut boost: u8) -> bool {
        // Same experimental gate as the CPU tier above.
        if boost == 3 && !self.allow_experimental {
            boost = 2;
        }
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x07, 0x03);
        report.args[0] = 0x01;
        report.args[1] = 0x02;
        report.args[2] = boost;
        if self.send_report(report).is_some() {
            return true;
        }
        false
    }

    pub fn set_power_mode(&mut self, mode: u8, cpu_boost: u8, gpu_boost: u8) -> bool {
        // Wire value is written verbatim (args[2] in set_power). Domain validity
        // and the exposed profile set are enforced upstream in the CLI; the EC
        // itself accepts any in-range value (verified: it does not enforce the
        // AC/DC domain, only rejects values > 7). Blade 16 2025 measured map:
        //   0 Balanced(AC)  2 Performance  3 BatterySaver(DC)  4 Custom
        //   5 Silent        6 Balanced(DC)
        // (1 = legacy ghost, 7 = HyperBoost — emitted only behind the
        // experimental unlock; the power-key cycle never emits either.)
        // Wire 7 = HyperBoost (Blade 18 native; 175 W cooling-pad state on the
        // 16). Refused unless explicitly unlocked — the single chokepoint every
        // caller funnels through (CLI, GUI, config restore, reapply).
        if mode == 7 && !self.allow_experimental {
            eprintln!("refusing profile 7 (HyperBoost) — enable experimental profiles in the About page");
            return false;
        }
        self.power = mode;
        // Chain every write result (external review 3.1). All steps ALWAYS
        // execute — plain `&&` over let-bindings, never a short-circuiting
        // call chain — so a failed early write cannot silently skip the later
        // ones and drift the EC even further from the requested profile.
        let ok;
        if mode == 4 {
            // Custom: mirror Synapse's MEASURED choreography (AC/DC captures
            // 2026-07): exactly four writes, NO reads — zone 1 profile, zone 2
            // profile, then CPU boost, then GPU boost. The former
            // read-before-write pattern was lineage-inherited: those captures
            // show Synapse writing straight through, and the read results were
            // discarded here anyway (never fed any logic). Boosts now land
            // after BOTH zones sit in the Custom slot, matching the wire.
            self.fan_rpm = 0;
            let zone1 = self.set_power(0x01);
            let zone2 = self.set_power(0x02);
            let cpu = self.set_cpu_boost(cpu_boost);
            let gpu = self.set_gpu_boost(gpu_boost);
            ok = zone1 && zone2 && cpu && gpu;
        } else {
            let zone1 = self.set_power(0x01);
            let zone2 = self.set_power(0x02);
            ok = zone1 && zone2;
        }

        ok
    }

    fn set_rpm(&mut self, zone: u8) -> bool {
        let mut report:RazerPacket = RazerPacket::new(0x0d, 0x01, 0x03);
        // Set fan RPM. profileId=1 matches Synapse's classId (Set Thermal Fan Speed).
        report.args[0] = 0x01;
        report.args[1] = zone;
        report.args[2] = self.fan_rpm;
        if self.send_report(report).is_some() {
            return true;
        }

        false
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

        zone1 && zone2 && fan1 && fan2
    }

    #[allow(dead_code)]
    pub fn get_fan_rpm(&mut self) -> u16 {
        let res: u16 = self.fan_rpm as u16;
        res * 100
    }

    /// Latch both fan zones into manual mode (fanModeValue=1) without setting a
    /// speed. Used by the curve task before its first speed write after a transition.
    pub fn set_fan_manual(&mut self) -> bool {
        let zone1 = self.set_zone_fan_state(0x01, 0x01);
        let zone2 = self.set_zone_fan_state(0x02, 0x01);
        zone1 && zone2
    }

    /// Set a single fan zone's RPM (clamped to the model range). Assumes the
    /// zone is already in manual mode.
    pub fn set_zone_rpm(&mut self, zone: u8, rpm: u16) -> bool {
        self.fan_rpm = self.clamp_fan(rpm);
        self.set_rpm(zone)
    }

    /// Read fan RPM from EC hardware.
    /// Note: on many Razer models this returns the configured target,
    /// not measured tachometer RPM (no tach register exposed via USB HID).
    pub fn read_fan_rpm_from_ec(&mut self) -> u16 {
        if let Some(rpm) = self.read_stored_fan_setpoint(0x01) {
            return rpm;
        }
        self.fan_rpm as u16 * 100
    }

    pub fn set_logo_led_state(&mut self, mode: u8) -> bool {
        if !self.static_lighting {
            return true; // lighting scope disabled (see set_standard_effect)
        }
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
        if self.send_report(report).is_some() {
            return true;
        }

        false
    }

    #[allow(dead_code)]
    pub fn get_logo_led_state(&mut self) -> u8 {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x82, 0x03);
        report.args[0] = RazerLaptop::VARSTORE;
        report.args[1] = RazerLaptop::LOGO_LED;
        if let Some(response) = self.send_report(report){
            return response.args[2];
        }
        0
    }

    pub fn set_brightness(&mut self, brightness: u8) -> bool {
        if !self.static_lighting {
            return true; // lighting scope disabled (see set_standard_effect)
        }
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x03, 0x03);
        report.args[0] = RazerLaptop::VARSTORE;
        report.args[1] = RazerLaptop::BACKLIGHT_LED;
        report.args[2] = brightness;
        if self.send_report(report).is_some() {
            return true;
        }

        false
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
        0
    }

    #[allow(dead_code)]
    pub fn get_bho(&mut self) -> Option<u8> {
        if !self.have_feature("bho".to_string()) {
            return None;
        }

        let mut report: RazerPacket = RazerPacket::new(0x07, 0x92, 0x01);
        report.args[0] = 0x00;

        self.send_report(report)
            .map(|resp| resp.args[0])
    }

    pub fn set_bho(&mut self, is_on: bool, threshold: u8) -> bool {
        if !self.have_feature("bho".to_string()) {
            return false;
        }

        let mut report = RazerPacket::new(0x07, 0x12, 0x01);
        report.args[0] = bho_to_byte(is_on, threshold);

        self.send_report(report)
            .is_some_and(|r| {
                // Multi-line packet dump demoted: it fired on every BHO write
                // including the boot restore (journal-diet consistency).
                log::debug!("BHO response packet: {:?}", r);
                true
            }
        )
    }

    fn next_transaction_id(&mut self) -> u8 {
        self.transaction_id = advance_tid(self.transaction_id);
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
/// EC fan clamp: outside the model range snaps to the nearer bound, inside
/// truncates to the EC's 100-RPM granularity (2250 -> 22 -> 2200 on the wire).
fn clamp_fan_to_range(rpm: u16, min: u16, max: u16) -> u8 {
    if rpm > max {
        return (max / 100) as u8;
    }
    if rpm < min {
        return (min / 100) as u8;
    }
    (rpm / 100) as u8
}

/// Synapse's transaction id cycles 1..=30 (measured, AC/DC captures 2026-07:
/// across 35 frames the ids run 0x01..0x1e globally monotonic and wrap
/// 0x1e -> 0x01). Neither 0x00 nor 0x1f ever appears on the wire, so this
/// emits neither — never emit what Synapse would not.
fn advance_tid(current: u8) -> u8 {
    let next = current + 1;
    if next > 30 { 1 } else { next }
}

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
    (u & (1 << 7) != 0, (u & 0b0111_1111))
}

fn bho_to_byte(is_on: bool, threshold: u8) -> u8 {
    if is_on {
        return threshold | 0b1000_0000;
    }
    threshold
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
    fn tid_cycles_one_to_thirty_and_wraps() {
        // Capture-derived invariant: 1..=30, wrap 30 -> 1, never 0 or 31.
        let mut id = 0u8; // daemon start state
        let mut seen = Vec::new();
        for _ in 0..61 {
            id = advance_tid(id);
            assert!((1..=30).contains(&id), "emitted {}", id);
            seen.push(id);
        }
        assert_eq!(seen[0], 1);
        assert_eq!(seen[29], 30);
        assert_eq!(seen[30], 1, "wrap 0x1e -> 0x01 as captured");
        assert!(!seen.contains(&0) && !seen.contains(&31));
    }

    #[test]
    fn fan_clamp_snaps_and_truncates() {
        // Blade 16 2025 range 2000..=5100 (laptops.json).
        assert_eq!(clamp_fan_to_range(0, 2000, 5100), 20, "below min snaps to min");
        assert_eq!(clamp_fan_to_range(1999, 2000, 5100), 20);
        assert_eq!(clamp_fan_to_range(2000, 2000, 5100), 20);
        assert_eq!(clamp_fan_to_range(2250, 2000, 5100), 22, "EC granularity truncates");
        assert_eq!(clamp_fan_to_range(5100, 2000, 5100), 51);
        assert_eq!(clamp_fan_to_range(65535, 2000, 5100), 51, "above max snaps to max");
    }

    fn curve() -> Vec<FanCurvePoint> {
        [(40u8, 2200u16), (50, 2600), (60, 3200)]
            .iter()
            .map(|&(temp_c, rpm)| FanCurvePoint { temp_c, rpm })
            .collect()
    }

    #[test]
    fn curve_lookup_is_a_ceiling_function() {
        let c = curve();
        assert_eq!(lookup_rpm(&c, 35.0), Some(2200), "below first point: curve floor");
        assert_eq!(lookup_rpm(&c, 45.0), Some(2600), "between points: next step up");
        assert_eq!(lookup_rpm(&c, 49.9), Some(2600));
        // Strictly-greater comparison: AT a point's temp the NEXT step applies.
        assert_eq!(lookup_rpm(&c, 50.0), Some(3200));
        assert_eq!(lookup_rpm(&c, 95.0), Some(3200), "above last point: last rpm");
        assert_eq!(lookup_rpm(&[], 50.0), None, "empty curve yields no target");
    }

    #[test]
    fn bho_byte_codec_roundtrips() {
        assert_eq!(bho_to_byte(true, 80), 0xD0, "top bit = enabled, low 7 = threshold");
        assert_eq!(bho_to_byte(false, 80), 0x50, "disable clears the bit, keeps threshold");
        assert_eq!(byte_to_bho(0xD0), (true, 80));
        // open-razerkit's Blade 16 2024 sends 0x41 for "off": decodes as
        // disabled with threshold 65 under this codec — same bit layout.
        assert_eq!(byte_to_bho(0x41), (false, 65));
        for on in [true, false] {
            for t in [50u8, 65, 80, 100] {
                assert_eq!(byte_to_bho(bho_to_byte(on, t)), (on, t));
            }
        }
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

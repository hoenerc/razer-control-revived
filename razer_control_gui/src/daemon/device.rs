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

impl SupportedDevice {
    /// laptops.json carries the pid as hex text ("02C6"); parse once. 0 on
    /// garbage = the strictest model surface (nothing stock beyond base).
    pub fn pid_u16(&self) -> u16 {
        u16::from_str_radix(&self.pid, 16).unwrap_or(0)
    }
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
        // WIRE ENDIANNESS: remaining_packets crosses the wire BIG-endian
        // (USBPcap + on-device probe: replies carry 00 01), but bincode
        // serializes u16 LITTLE-endian. Swap the two bytes at the HID
        // boundary so the struct keeps normal integer semantics while the
        // wire sees Synapse-identical bytes. The CRC is an XOR over [2..88]
        // and therefore swap-invariant.
        buf.swap(3, 4);
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
    /// The charger domain of the most recent successful apply (runtime only,
    /// never persisted). Source of truth for "did the domain change since the
    /// last apply" — deliberately NOT the EC profile read-back, which was
    /// measured returning stale/contradictory values after writes (2026-07-20).
    active_domain: Option<ChargerDomain>,
    /// The wire value of the most recent live EC profile apply (runtime only).
    /// The power-key advance steps from here; every apply path records it, so
    /// it is exact as long as all writes go through this manager — which they
    /// do by design (one door).
    last_applied_wire: Option<u8>,
    /// Timestamp AND charger domain of the last SUCCESSFUL power-key press.
    /// The warm window is bound to the previous PRESS's domain (v2.14.1) —
    /// binding it to the last apply's domain let an event sync inside the 3 s
    /// window (unplug right after a press) turn the next press warm although
    /// the documented contract demands cold on any domain change.
    last_key_press: Option<(time::Instant, ChargerDomain)>,
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
            active_domain: None,
            last_applied_wire: None,
            last_key_press: None,
        }
    }

    pub fn read_laptops_file() -> io::Result<DeviceManager > {
        let path = device_file_path();
        let str: Vec<u8> = fs::read(&path)?;
        let mut res: DeviceManager = DeviceManager::new();
        res.supported_devices = serde_json::from_slice(str.as_slice())?;
        println!("suported devices found: {:?}", res.supported_devices.len());
        match config::Configuration::read_from_config() {
            Ok(mut c) => {
                if sanitize_loaded_config(&mut c, None) {
                    // Union pass: no device is attached yet, so only values
                    // invalid on EVERY model fall here; the model tightening
                    // runs at attach (enforce_model_law). Persist the repair
                    // once so it cannot replay on the NEXT boot either.
                    if let Err(e) = c.write_to_file() {
                        eprintln!("could not persist the sanitized config: {e}");
                    }
                }
                res.config = Some(c);
            }
            // NotFound: genuine first start. InvalidData: content-level
            // garbage — quarantine() already left its unmissable line.
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData) => {
                res.config = Some(config::Configuration::new());
            }
            Err(e) => {
                // Environment-level failure (permissions, I/O, a failed
                // migration write): the file may be perfectly intact, so say
                // so instead of impersonating a first start.
                eprintln!(
                    "CONFIG UNREADABLE: could not load daemon.json ({e}) — running on defaults \
                     this session; fix the underlying error and restart, or the next settings \
                     change will overwrite the stored file with defaults"
                );
                res.config = Some(config::Configuration::new());
            }
        }

        Ok(res)
    }

    /// The model tightening that load-time sanitizing cannot do: the PID is
    /// only known once a device attached. Re-run the sanitizer with the real
    /// model and persist a repair once — after this, the boot restore can
    /// only ever replay state that is valid ON THIS MODEL.
    fn enforce_model_law(&mut self) {
        let pid = self.device.as_ref().map(|d| d.get_pid());
        if let (Some(pid), Some(cfg)) = (pid, self.config.as_mut()) {
            if sanitize_loaded_config(cfg, Some(pid)) {
                if let Err(e) = cfg.write_to_file() {
                    eprintln!("could not persist the model-sanitized config: {e}");
                }
            }
        }
    }

    /// The EFFECTIVE surface for clients: model and experimental flag already
    /// applied. Single source — GUI, CLI and the power-key cycle must derive
    /// from this instead of hardcoding profile lists.
    pub fn get_capabilities(&mut self, ac: usize) -> (Vec<u8>, u8, String) {
        let (turbo_stock, max_stock) = self
            .device
            .as_ref()
            .map_or((false, false), |d| (d.turbo_is_stock(), d.max_tier_is_stock()));
        let model = self
            .device
            .as_ref()
            .map_or_else(|| String::from("no device"), |d| d.get_name());
        let experimental = self
            .config
            .as_ref()
            .is_some_and(|c| c.experimental_profiles);
        let wires = effective_profiles(turbo_stock, ac == 1, experimental);
        let max_tier = if experimental || max_stock { 3 } else { 2 };
        (wires, max_tier, model)
    }

    fn get_ac_config(&mut self, ac: usize) -> Option<config::PowerConfig> {
        if let Some(c) = self.get_config() {
            return Some(c.power[ac].clone());
        }

        None
    }

    pub fn light_off(&mut self) {
        // Handover guarantee: with static_lighting off, even the sleep
        // blank-out belongs to the external RGB tool — zero writes.
        if !self.get_static_lighting() {
            return;
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_brightness(0);
            laptop.set_logo_led_state(0);
        }
    }

    pub fn restore_light(&mut self) {
        if !self.get_static_lighting() {
            return; // handover guarantee — see light_off
        }
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
        if self.get_device().is_none() {
            return false;
        }
        // The freshly resolved domain picks everything, and the pick itself
        // is the single desired-state evaluation (Pd => wire 0, slot and
        // boost rules live THERE, once) — this path only consumes it.
        let domain = self.resolve_domain();
        let desired = match self.desired_state_now(domain) {
            Some(d) => d,
            None => return false,
        };
        if let Some((cpu, gpu)) = desired.boosts {
            println!(
                "Re-applying power profile: domain={:?} power_mode={} cpu_boost={} gpu_boost={}",
                domain, desired.wire, cpu, gpu
            );
        } else {
            // Boost bytes only ever go out for Custom — printing them for
            // other wires forged values the EC never saw (boost-1 case).
            println!("Re-applying power profile: domain={:?} power_mode={}", domain, desired.wire);
        }
        // Non-Custom: the boost ARGUMENTS are inert (bytes never sent) — pass
        // zeros instead of hauling slot values that would only feign meaning.
        let (cpu, gpu) = desired.boosts.unwrap_or((0, 0));
        let result = match self.get_device() {
            Some(laptop) => laptop.set_power_mode(desired.wire, cpu, gpu),
            None => false,
        };
        if result {
            self.last_applied_wire = Some(desired.wire);
            self.active_domain = Some(domain);
        }
        // The power-mode reapply rewrote the fan-state command; re-latch the curve
        // now instead of waiting for the next tick (avoids the resume-burst rattle).
        if !self.reassert_fan_curve() {
            eprintln!("power profile re-applied but the fan-curve re-assert failed — the curve retries on its next tick");
        }
        result
    }

    /// Desired-state snapshot for the socket. None without a device (the domain would
    /// be guesswork, never phantasized from mirrors) or without a config.
    pub fn desired_state_wire(&mut self) -> Option<crate::comms::DesiredStateWire> {
        self.device.as_ref()?;
        let lighting = self.get_static_lighting();
        let domain = self.resolve_domain();
        self.desired_state_now(domain).map(|d| {
            let (fan_mode, fan_rpm) = match d.fan {
                FanTarget::Auto => (0u8, 0i32),
                FanTarget::Manual(r) => (1, r),
                FanTarget::Curve => (2, 0),
            };
            crate::comms::DesiredStateWire {
                domain: domain.to_wire(),
                wire: d.wire,
                boosts: d.boosts,
                brightness: d.brightness,
                logo: d.logo,
                fan_mode,
                fan_rpm,
                bho_on: d.bho.0,
                bho_threshold: d.bho.1,
                lighting,
            }
        })
    }

    /// Actual-side accessors: live EC diagnostics under the daemon's HID lock. All
    /// return None on EC silence — never a fake value.
    pub fn ec_power_zone(&mut self, zone: u8) -> Option<(u8, u8)> {
        self.get_device().and_then(|l| l.read_zone_fan_state(zone))
    }
    pub fn ec_boost(&mut self, gpu: bool) -> Option<u8> {
        self.get_device()
            .and_then(|l| if gpu { l.get_gpu_boost() } else { l.get_cpu_boost() })
    }
    pub fn ec_brightness(&mut self) -> Option<u8> {
        self.get_device().and_then(|l| l.get_brightness())
    }
    pub fn ec_bho(&mut self) -> Option<(bool, u8)> {
        self.get_device().and_then(|l| l.get_bho()).map(byte_to_bho)
    }
    pub fn ec_fan_tach(&mut self, zone: u8) -> Option<u16> {
        self.get_device().and_then(|l| l.read_fan_tach(zone))
    }
    pub fn ec_fan_setpoint(&mut self, zone: u8) -> Option<u16> {
        self.get_device().and_then(|l| l.read_stored_fan_setpoint(zone))
    }

    /// Raw ACTP byte from the EC, for the CLI passthrough (`read charger`).
    /// None on EC read failure. No classification — the CLI prints the value
    /// verbatim as hex so scripts see exactly what the EC reports.
    pub fn read_charger_domain_raw(&mut self) -> Option<u8> {
        self.get_device().and_then(|l| l.read_charger())
    }

    /// Read the EC charger class and classify it into a ChargerDomain. None if
    /// the device is missing or the EC read failed — callers decide the
    /// fallback (the UPower-driven AC/DC path stays valid).
    pub fn read_charger_domain(&mut self) -> Option<ChargerDomain> {
        let actp = self.read_charger_domain_raw()?;
        Some(ChargerDomain::from_actp(actp))
    }

    /// Apply the profile the given charger domain calls for, reading the EC for
    /// ground truth instead of trusting the config cache. This is the single
    /// heal/restore entry the charger-aware triggers funnel through
    /// (startup, resume-settle, power key, hot-swap recovery):
    ///
    ///   Barrel  -> restore the stored AC profile (slot 1) in full
    ///   Battery -> restore the stored DC profile (slot 0) in full
    ///   Pd      -> force wire 0 (Balanced); the EC caps the dGPU on PD anyway,
    ///              so Balanced is safe. NO write into the AC slot — a PD
    ///              episode must never clobber the user's stored AC choice
    ///              (Synapse-faithful: Silent survived two PD episodes in the
    ///              captures). Stored Custom boosts stay untouched for the same
    ///              reason: PD never carries the boost re-assert.
    ///
    /// Returns the wire actually applied so the caller (power key) can render an
    /// OSD result rather than an assumed request.
    /// The single desired-state evaluation against the live config. None only when no
    /// config is loaded (callers already guard that case).
    fn desired_state_now(&self, domain: ChargerDomain) -> Option<DesiredState> {
        self.config.as_ref().map(|c| desired_state(c, domain))
    }

    pub fn apply_charger_domain(&mut self, domain: ChargerDomain) -> Option<u8> {
        let applied = match domain {
            ChargerDomain::Pd => {
                // Seed the mirrors without any profile apply, then the
                // volatile PD surface: Balanced live, NOTHING persisted.
                // Brightness/logo still follow the AC slot for Synapse parity.
                self.set_ac_mirror(true);
                let lighting = self.get_static_lighting();
                if let Some(desired) = self.desired_state_now(ChargerDomain::Pd) {
                    if let Some(laptop) = self.get_device() {
                        if lighting {
                            laptop.set_brightness(desired.brightness);
                            laptop.set_logo_led_state(desired.logo);
                        } else {
                            println!("PD aux lighting skipped (static_lighting off)");
                        }
                    }
                }
                let ok = self
                    .get_device()
                    .map(|l| l.set_power_mode(0, 0, 0))
                    .unwrap_or(false);
                if !self.reassert_fan_curve() {
                    eprintln!("charger-domain apply: fan-curve re-assert failed — the curve retries on its next tick");
                }
                ok.then_some(0u8)
            }
            ChargerDomain::Barrel | ChargerDomain::Battery => {
                // The existing binary path IS the full correct apply for these
                // two domains (mirrors, slot restore, fan re-latch) — reuse it
                // wholesale instead of duplicating it here.
                let idx = domain.config_index();
                let wire = self.desired_state_now(domain).map(|d| d.wire);
                // v2.14.1: only a CONFIRMED profile write may claim the wire —
                // set_ac_state now reports the power write's own result, so an
                // unconfirmed apply leaves the bookkeeping (and the OSD) alone.
                if self.set_ac_state(idx == 1) { wire } else { None }
            }
        };
        // Runtime bookkeeping: the daemon is its own source of truth for "what
        // did I last apply, and in which domain" — the EC profile read-back was
        // falsified as a decision basis (stale/contradictory after writes).
        if let Some(wire) = applied {
            if self.active_domain != Some(domain) {
                // A confirmed domain CHANGE invalidates the warm window: a
                // barrel->battery->barrel excursion inside 3 s would otherwise
                // read as warm although the domain moved since the last press
                // (review finding; cheapest correct fix).
                self.last_key_press = None;
            }
            self.active_domain = Some(domain);
            self.last_applied_wire = Some(wire);
        }
        applied
    }

    /// Resolve the current charger domain for USER-ACTION paths (key press,
    /// socket write): one fresh EC read, falling back to the tracked domain,
    /// falling back to the binary mirror. Plug EVENTS keep using
    /// sync_charger_domain, whose UPower-first logic and stale-echo retry
    /// exist precisely because the EC lies for ~a second around plug changes;
    /// user actions rarely race that window (documented residual gap).
    fn resolve_domain(&mut self) -> ChargerDomain {
        if let Some(domain) = self.read_charger_domain() {
            return domain;
        }
        // v2.14.2: on a failed EC read, NEVER fall back to the tracked domain
        // — after an eventless barrel<->PD hot-swap it can still say Barrel
        // and would apply the full AC ladder under PD (review finding). Same
        // fail-safe as the event path: offline => Battery, online-but-
        // unresolved => PD, the smallest surface. Momentarily demoting a real
        // barrel to Balanced is conservative and self-healing; promoting an
        // unknown online source to Barrel is exactly the wrong direction.
        let plugged = self
            .get_device()
            .map(|l| l.get_ac_state() == 1)
            .unwrap_or(true);
        if plugged { ChargerDomain::Pd } else { ChargerDomain::Battery }
    }

    /// Power-key action, cold/warm semantics (operator design, 2026-07-20):
    ///
    ///   COLD press (no successful press within the advance window, or the
    ///   charger domain changed since): (re-)apply the profile that belongs to
    ///   the CURRENT power state — barrel: the stored AC profile, battery: the
    ///   stored DC profile, USB-PD: Balanced. This is byte-identical to the
    ///   event sync, so the first press is simultaneously a confirmation
    ///   ("you are on Silent"), a re-assert, and — on any drift or hot-swap —
    ///   the heal. Healing stops being a special case.
    ///
    ///   WARM press (again within POWER_KEY_ADVANCE_WINDOW_MS, same domain):
    ///   advance one step in the domain's cycle, sliding the window, so
    ///   mashing the key still cycles quickly. Under PD the cycle has one
    ///   element, so warm == cold == Balanced.
    ///
    /// The cycle position steps from `last_applied_wire` (the daemon's own
    /// bookkeeping — every write goes through this one door), NOT from an EC
    /// read-back: the 0x0d/0x82 read was measured returning stale and
    /// contradictory values after profile writes and is banned from decisions.
    ///
    /// Returns (applied_wire, domain, cold) for OSD rendering. EC round-trips
    /// happen under the manager lock but never across the D-Bus/OSD call —
    /// the caller renders after this returns.
    pub fn cycle_power_key(&mut self) -> Option<(u8, ChargerDomain, bool)> {
        let domain = self.resolve_domain();
        let now = time::Instant::now();
        let warm = self
            .last_key_press
            .map(|(t, d)| {
                d == domain
                    && now.duration_since(t)
                        <= time::Duration::from_millis(POWER_KEY_ADVANCE_WINDOW_MS)
            })
            .unwrap_or(false);

        if !warm {
            // Cold press == manual sync: apply the domain's proper profile.
            // apply_charger_domain seats the mirror, records the bookkeeping,
            // and for barrel/battery restores the FULL stored config —
            // including Custom with its stored boosts (re-asserting the
            // user's own choice is restore parity, not "landing in Custom").
            let wire = self.apply_charger_domain(domain)?;
            self.last_key_press = Some((now, domain));
            return Some((wire, domain, true));
        }

        // Warm press: advance one step.
        if domain == ChargerDomain::Pd {
            // Single-element surface: the advance IS the re-assert.
            let wire = self.apply_charger_domain(domain)?;
            self.last_key_press = Some((now, domain));
            return Some((wire, domain, false));
        }

        let idx = domain.config_index();
        let turbo_stock = self.device.as_ref().is_some_and(|d| d.turbo_is_stock());
        let experimental = self.config.as_ref().is_some_and(|c| c.experimental_profiles);
        // The exposed cycle: effective surface minus Custom(4) and Gaming(1) —
        // a stray key must never CHANGE INTO a tuned or legacy state (the cold
        // press may re-assert Custom, the warm press always leaves it).
        let order: Vec<u8> = effective_profiles(turbo_stock, idx == 1, experimental)
            .into_iter()
            .filter(|w| !CYCLE_EXCLUDED_WIRES.contains(w))
            .collect();
        if order.is_empty() {
            return None;
        }
        let next = match self
            .last_applied_wire
            .and_then(|cur| order.iter().position(|&v| v == cur))
        {
            Some(pos) => order[(pos + 1) % order.len()],
            // Custom/ghost or unknown position: go to the domain's Balanced.
            None => order[0],
        };

        // Preserve stored Custom boosts across the write.
        let cpu = self.get_ac_config(idx).map(|c| c.cpu_boost).unwrap_or(0);
        let gpu = self.get_ac_config(idx).map(|c| c.gpu_boost).unwrap_or(0);
        if !self.set_power_mode_in_domain(idx, next, cpu, gpu, domain) {
            return None;
        }
        self.last_key_press = Some((now, domain));
        Some((next, domain, false))
    }

    /// Socket entry (CLI/GUI): resolve the charger domain fresh, then run the
    /// domain-aware chokepoint. One EC read per write — writes are rare, and
    /// this is what makes EVERY door heal, not just the power key.
    pub fn set_power_mode(&mut self, ac: usize, pwr: u8, cpu: u8, gpu: u8) -> bool {
        let domain = self.resolve_domain();
        self.set_power_mode_in_domain(ac, pwr, cpu, gpu, domain)
    }

    /// The domain-aware write chokepoint. `domain` is the caller's freshly
    /// resolved charger domain (the power key passes its own resolve so a warm
    /// press costs a single EC read, not two).
    fn set_power_mode_in_domain(
        &mut self,
        ac: usize,
        pwr: u8,
        cpu: u8,
        gpu: u8,
        domain: ChargerDomain,
    ) -> bool {
        // Heal-first on drift: if the live domain differs from the last
        // applied one (hot-swap without a Linux event, or nothing applied
        // yet), re-seat the whole state before processing the request —
        // otherwise the slot gate below compares against a stale mirror and
        // silently defers live writes (observed 2026-07-20).
        if self.active_domain != Some(domain) {
            println!("power write: domain drift detected — syncing to {:?} first", domain);
            if self.apply_charger_domain(domain).is_none() {
                eprintln!(
                    "power write: drift heal to {:?} failed — rejecting the request rather than writing onto an unconfirmed base",
                    domain
                );
                return false;
            }
        }
        // Validate BEFORE anything persists: a rejected request must leave no
        // trace — not in the config file (the boot restore would replay it
        // forever) and not on the EC. The CLI and GUI never send these, so a
        // rejection here means a raw socket client or a bug; either way the
        // journal names the request and the reason. No device attached means
        // pid 0 = the strictest surface (nothing stock beyond base).
        let pid = self.device.as_ref().map_or(0, |d| d.get_pid());
        let experimental = self
            .config
            .as_ref()
            .is_some_and(|c| c.experimental_profiles);
        if let Err(reason) = validate_power_request(pid, ac, pwr, cpu, gpu, experimental) {
            eprintln!(
                "rejected SetPowerMode {{ ac: {ac}, pwr: {pwr}, cpu: {cpu}, gpu: {gpu} }}: {reason}"
            );
            return false;
        }
        let mut res: bool = false;
        // The power-mode command rewrites the per-zone fan-state, so re-assert
        // manual fan mode on the next curve tick.
        self.fan_curve_established = false;
        // Boosts are Custom-only parameters (see DesiredState): outside
        // wire 4 the bytes never reach the EC, and the slot's stored boosts
        // are the user's Custom tuning — a plain profile write must not
        // clobber them (the CLI fills its non-optional IPC fields with 0,0).
        let (cpu, gpu) = if pwr == 4 {
            (cpu, gpu)
        } else {
            self.get_ac_config(ac)
                .map(|c| (c.cpu_boost, c.gpu_boost))
                .unwrap_or((cpu, gpu))
        };
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
            } else if domain == ChargerDomain::Pd && pwr != 0 {
                // USB-PD: the live surface is {Balanced}. The request is a
                // legitimate choice FOR THE AC SLOT and is stored above; the
                // EC stays pinned to Balanced until a real AC session
                // (Synapse-faithful — measured 2026-07-20). Balanced itself
                // (pwr == 0) falls through and applies as a re-assert.
                println!(
                    "power write: stored wire {pwr} for the AC slot; USB-PD active, EC stays on Balanced"
                );
                res = true;
            } else {
                res = laptop.set_power_mode(pwr, cpu, gpu);
                if res {
                    self.last_applied_wire = Some(pwr);
                    self.active_domain = Some(domain);
                }
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
        let mut saved = true;
        if let Some(config) = self.get_config() {
            config.power[ac].fan_rpm = rpm;
            config.power[ac].fan_curve.enabled = false;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
                saved = false;
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

        res && saved
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
                    // Same wrap guard as the restore path in set_config: the
                    // persisted value is i32 and could predate validation.
                    match u16::try_from(fan_rpm) {
                        Ok(rpm) => {
                            laptop.set_fan_rpm(rpm);
                        }
                        Err(_) => eprintln!(
                            "curve-off fan restore skipped: persisted rpm {fan_rpm} not representable"
                        ),
                    }
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
        if logo_state > 2 {
            eprintln!(
                "rejected SetLogoLedState {{ ac: {ac}, logo_state: {logo_state} }}: valid states are 0..=2"
            );
            return false;
        }
        let mut res: bool = false;
        
        let mut saved = true;
        if let Some(config) = self.get_config() {
            config.power[ac].logo_state = logo_state;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
                saved = false;
            }
        }
        let lighting = self.get_static_lighting();
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if !lighting {
                println!("logo stored only — lighting writes disabled (static_lighting off)");
                res = true;
            } else if state != ac {
                res = true;
            } else {
                res = laptop.set_logo_led_state(logo_state);
            }
        }

        res && saved
    }

    pub fn get_logo_led_state(&mut self, ac: usize) -> u8 {
        // Config-backed BY DESIGN: the 2025 EC answers NOT_SUPPORTED to the
        // whole logo state/brightness channel in both directions (probe
        // 2026-07-20, both varstores), so there is nothing to read and the
        // tool deliberately never asks — asking would only manufacture
        // errors. The stored value is what the write path last mirrored.
    
        if let Some(config) = self.get_ac_config(ac) {
            return config.logo_state;
        }

        0
    }

    pub fn set_brightness(&mut self, ac:usize, brightness: u8) -> bool {
        let mut res: bool = false;
        let clamped = if brightness > 100 { 100u16 } else { brightness as u16 };
        let _val = clamped * 255 / 100;
        
        let mut saved = true;
        if let Some(config) = self.get_config() {
            config.power[ac].brightness = _val as u8;
            if let Err(e) = config.write_to_file() {
                eprintln!("Error write config {:?}", e);
                saved = false;
            }
        }
        let lighting = self.get_static_lighting();
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if !lighting {
                println!("brightness stored only — lighting writes disabled (static_lighting off)");
                res = true;
            } else if state != ac {
                res = true;
            } else {
                res = laptop.set_brightness(_val as u8);
            }
        }

        res && saved
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
        // -1 = no confirmed tach reply (this wire type predates Option).
        self.get_device()
            .and_then(|laptop| laptop.read_fan_rpm_from_ec())
            .map(|v| v as i32)
            .unwrap_or(-1)
    }

    pub fn get_fan_rpm(&mut self, ac: usize) -> i32 {
        // Pure parameter read (source-purity ruling): the stored slot value,
        // nothing else — the live value is `read fan ec` (zone-1 tachometer,
        // `0x0d/0x88`).
        self.get_ac_config(ac).map(|c| c.fan_rpm).unwrap_or(0)
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

    /// Seed the binary AC mirror (plus the experimental/lighting mirrors and
    /// the fan-curve latch) WITHOUT applying any profile. The charger-aware
    /// triggers use this so the subsequent sync_charger_domain performs the
    /// single, correct apply — going through set_ac_state instead would first
    /// apply the binary slot and let a PD plug-in flash the stored AC profile
    /// onto the EC for a moment before the correction lands.
    pub fn set_ac_mirror(&mut self, online: bool) {
        self.fan_curve_established = false;
        if let (Some(dev), Some(cfg)) = (self.device.as_mut(), self.config.as_ref()) {
            dev.allow_experimental = cfg.experimental_profiles;
            dev.static_lighting = cfg.static_lighting;
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_ac_state(online);
        }
    }

    /// Returns true iff the PROFILE write was confirmed (auxiliaries are
    /// best-effort; see RazerLaptop::set_config). Callers that only re-seat
    /// state may ignore the result; the domain apply must not.
    pub fn set_ac_state(&mut self, ac: bool) -> bool {
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
        let mut power_ok = false;
        if let Some(config) = config {
            let lighting = self.get_static_lighting();
            if let Some(laptop) = self.get_device() {
                power_ok = laptop.set_config(config, lighting);
            }
        }
        if !self.reassert_fan_curve() {
            eprintln!("AC/battery switch: fan-curve re-assert failed — the curve retries on its next tick");
        }
        power_ok
    }

    /// Read the AC0 online flag straight from UPower (same proxy the binary
    /// path uses). None if D-Bus/UPower is unavailable.
    fn upower_online(&self) -> Option<bool> {
        let dbus_system = Connection::new_system().ok()?;
        let proxy_ac = dbus_system.with_proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/line_power_AC0",
            time::Duration::from_millis(5000),
        );
        use crate::battery::OrgFreedesktopUPowerDevice;
        proxy_ac.online().ok()
    }

    /// Charger-aware domain sync. The kernel's Mains bit is AUTHORITATIVE for
    /// battery: online=false means the adapter is gone, no EC read needed —
    /// and right after an unplug the EC's first 0x8c read still ECHOES the
    /// previous value (measured), so asking it then actively misleads. The EC
    /// read is only consulted when online=true, to split barrel from PD, with
    /// a bounded retry against the same stale-echo window; if the EC keeps
    /// contradicting the kernel (0x00 while online), the source is treated as
    /// PD — the constrained domain, safe on every adapter. Used by every
    /// trigger (startup, AC0 signal, resume-settle). If UPower AND the EC are
    /// both unreadable it falls back to the old binary path so behaviour never
    /// regresses below the pre-charger baseline.
    pub fn sync_charger_domain(&mut self) {
        let domain = match self.upower_online() {
            Some(false) => Some(ChargerDomain::Battery),
            Some(true) => {
                let mut resolved = None;
                for attempt in 0..3 {
                    if attempt > 0 {
                        thread::sleep(time::Duration::from_millis(400));
                    }
                    match self.read_charger_domain() {
                        Some(ChargerDomain::Battery) => {
                            // EC says "no adapter" while the kernel says
                            // online: stale echo or contradiction — retry,
                            // and if it persists, fail safe to PD (Balanced,
                            // volatile) rather than the full AC ladder.
                            resolved = Some(ChargerDomain::Pd);
                        }
                        Some(d) => {
                            resolved = Some(d);
                            break;
                        }
                        None => {
                            // Transport failure: keep retrying inside the same
                            // bounded window — a transient read error must not
                            // escape to the binary fallback below, which would
                            // apply the full AC ladder under a PD source.
                        }
                    }
                }
                if resolved.is_none() {
                    // online=true with the EC still unresolved after the
                    // window: fail safe to PD (smallest surface).
                    eprintln!("charger sync: online but EC unresolved after retries — failing safe to PD");
                    resolved = Some(ChargerDomain::Pd);
                }
                resolved
            }
            None => self.read_charger_domain(),
        };
        match domain {
            Some(domain) => {
                let applied = self.apply_charger_domain(domain);
                println!(
                    "charger sync: domain={:?} applied_wire={:?}",
                    domain, applied
                );
            }
            None => {
                // EC unreadable — keep the old binary behaviour, but say so:
                // a silently failing charger read must be visible in the journal.
                eprintln!("charger sync: EC charger read failed — falling back to the binary UPower path");
                self.set_ac_state_get();
            }
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
                let lighting = self.get_static_lighting();
                if let Some(laptop) = self.get_device() {
                    laptop.set_config(config, lighting);
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
        // Bit 7 of the wire byte is the on/off flag (see bho_to_byte): an
        // out-of-range threshold would silently corrupt the request. The CLI
        // enforces this range already; the daemon boundary must too.
        if !bho_threshold_valid(threshold) {
            eprintln!(
                "rejected SetBatteryHealthOptimizer {{ is_on: {is_on}, threshold: {threshold} }}: threshold must be 50..=80"
            );
            return false;
        }
        let result = self.get_device()
            .is_some_and(|laptop| laptop.set_bho(is_on, threshold));
        let mut saved = true;
        if result {
            if let Some(config) = self.get_config() {
                config.bho_on = is_on;
                config.bho_threshold = threshold;
                if let Err(e) = config.write_to_file() {
                    eprintln!("Error write config {:?}", e);
                    saved = false;
                }
            }
        }
        result && saved
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
                                    supported_device.pid_u16(),
                                    dev,
                                ));
                                self.enforce_model_law();
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
                                    supported_device.pid_u16(),
                                    dev,
                                ));
                                self.enforce_model_law();
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
    /// USB product id (parsed from laptops.json); keys the per-model
    /// capability matrix — what is STOCK here vs. behind the opt-in.
    pid: u16,
    device: hidapi::HidDevice,
    power: u8, // need for fan
    fan_rpm: u8, // need for power
    ac_state: u8, // index config array
    transaction_id: u8,
    /// Config-backed experimental unlock; gates the HyperBoost chokepoint and
    /// the CPU/GPU boost-tier caps. Mirrored by the DeviceManager.
    pub(crate) allow_experimental: bool,
    pub(crate) static_lighting: bool,
    /// Commands the EC answered with NOT_SUPPORTED this session — used ONLY
    /// to deduplicate the journal line (first reject loud, later ones at
    /// debug). The writes themselves always keep going out: write parity
    /// means Synapse's values land in Synapse's registers even when the EC
    /// answers 0x05 — it does so for Synapse's own logo-state writes too
    /// (USBPcap 2026-07-20), and whether the EC honours them despite the
    /// status code is a separate, open question.
    unsupported_cmds: Vec<(u8, u8)>,
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

    pub fn new(name: String, features: Vec<String>, fan: Vec<u16>, pid: u16, device: hidapi::HidDevice) -> RazerLaptop {
        RazerLaptop{
            name,
            features,
            fan,
            device,
            power: 0,
            fan_rpm: 0,
            ac_state: 0,
            transaction_id: 0,
            pid,
            allow_experimental: false,
            static_lighting: true,
            unsupported_cmds: Vec::new(),
        }
    }

    /// Apply a full power-config to the hardware. Returns the POWER write's
    /// own result (v2.14.1): brightness, logo and fan stay best-effort
    /// auxiliaries — the old `ret |=` aggregation let a successful brightness
    /// write vouch for a failed profile write, which would have poisoned the
    /// domain bookkeeping that now feeds heals and the key cycle.
    pub fn set_config(&mut self, config: config::PowerConfig, lighting: bool) -> bool {
        if lighting {
            let _ = self.set_brightness(config.brightness);
            let _ = self.set_logo_led_state(config.logo_state);
        } else {
            println!("lighting restore skipped (static_lighting off)");
        }
        let power_ok = self.set_power_mode(config.power_mode, config.cpu_boost, config.gpu_boost);
        // When a smart curve owns the fans, leave the speed to the curve task so
        // an AC/profile switch doesn't briefly drop the fans to auto/fixed.
        if !config.fan_curve.enabled {
            // Persisted value is i32; a negative or oversized survivor would
            // silently WRAP through `as u16` (-1 -> 65535). Restore only what
            // is representable; anything else is journaled and skipped — the
            // validated setter is the only way such a value could be fixed.
            match u16::try_from(config.fan_rpm) {
                Ok(rpm) => {
                    let _ = self.set_fan_rpm(rpm);
                }
                Err(_) => eprintln!(
                    "fan restore skipped: persisted rpm {} not representable (config predates validation?)",
                    config.fan_rpm
                ),
            }
        }
        power_ok
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

    pub fn get_pid(&self) -> u16 {
        self.pid
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

    /// Read the power adapter class the EC currently sees, via `0x07/0x8c`
    /// (GET variant of `0x0c`; command class 0x07 = battery/charger). The value
    /// lands in args[0]; args[1] is a constant 0x11 across all states [O].
    ///
    /// Measured value map (Blade 16 2025, USBPcap + on-device EC RAM read of
    /// field ACTP @ 0xEB, 2026-07-19/20):
    ///   0x00 none / rejected (< ~35 W PD, or on battery)
    ///   0x02 45 W PD · 0x03 60 W PD (3 A cable) · 0x04 65 W PD · 0x07 100 W PD
    ///   0x11 barrel (proprietary DC jack)
    /// The EC reports the ACTIVE source: with barrel + PD both attached it
    /// returns 0x11 (barrel wins; no bitmask OR). This is why software cannot
    /// tell PD from barrel via ACPI — the kernel only sees AC0/online=1 for
    /// every one of these — and why the charger domain needs this EC read.
    ///
    /// Returns None on transport failure so callers can fall back to the
    /// UPower AC/DC split rather than treating a missing read as "no adapter".
    pub fn read_charger(&mut self) -> Option<u8> {
        // Request args are all zero; the reply carries the class in args[0].
        // A freshly-attached source can echo the previous value on the first
        // read or two before settling — callers on a plug EVENT should read
        // after their existing settle window, never blind-once.
        let report: RazerPacket = RazerPacket::new(0x07, 0x8c, 0x02);
        self.send_report(report).map(|response| response.args[0])
    }

    /// Zone read via `0x0d/0x82`: (performance wire, fan-state byte).
    /// BANNED from decision paths since 2026-07-20: measured returning stale
    /// and mutually contradictory values after profile writes (fresh-correct
    /// at 2 s in one run, minutes-old values in another, sporadic None). The
    /// daemon's own apply bookkeeping (`last_applied_wire`) is the decision
    /// basis; this stays available as a diagnostic only.
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

    /// Per-zone fan tachometer via `0x0d/0x88` — a real tach DOES exist on
    /// the 2025 EC (probe-measured 2026-07-22: both zones answer 0 rpm at
    /// standstill; the old "no tach register" note came from other models).
    /// Scaling assumed ×100 like the 0x81 setpoint; verify once under load.
    fn read_fan_tach(&mut self, zone: u8) -> Option<u16> {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x88, 0x04);
        report.args[0] = 0x01;
        report.args[1] = zone;
        self.send_report(report)
            .map(|response| response.args[2] as u16 * 100)
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

    pub     fn set_power(&mut self, zone: u8) -> bool {
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
    /// Live EC boost read (0x87, CPU zone), a `read boost cpu ec` diagnostic.
    /// None on EC silence — a failed read must NEVER collapse to 0, which is
    /// a valid boost tier.
    pub fn get_cpu_boost(&mut self) -> Option<u8> {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x87, 0x03);
        report.args[0] = 0x01;
        report.args[1] = 0x01;
        report.args[2] = 0x00;
        self.send_report(report).map(|response| response.args[2])
    }

    fn set_cpu_boost(&mut self, mut boost: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x07, 0x03);
        // Tier 3 = "Max", the canonical Synapse name (stock on the Blade 18,
        // opt-in elsewhere). Reachable despite the request validator: a live
        // experimental DISABLE leaves tier 3 in the stored config until the
        // attach-time sanitizer runs; the reapply path lands here in between.
        // Say so instead of silently writing something else than the config
        // claims.
        if boost == 3 && !(self.allow_experimental || self.max_tier_is_stock()) {
            eprintln!("CPU boost tier 3 (Max) without an unlock on this model — clamped to 2 for this write");
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

    /// Live EC boost read (0x87, GPU zone), a `read boost gpu ec` diagnostic.
    /// Same Option contract as the CPU read.
    pub fn get_gpu_boost(&mut self) -> Option<u8> {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x87, 0x03);
        report.args[0] = 0x01;
        report.args[1] = 0x02;
        report.args[2] = 0x00;
        self.send_report(report).map(|response| response.args[2])
    }

    fn set_gpu_boost(&mut self, mut boost: u8) -> bool {
        // Same model-aware gate as the CPU tier above.
        if boost == 3 && !(self.allow_experimental || self.max_tier_is_stock()) {
            eprintln!("GPU boost tier 3 (Max) without an unlock on this model — clamped to 2 for this write");
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
        // (1 = legacy ghost, 7 = Turbo — Turbo is STOCK on the Blade 18 and
        // opt-in elsewhere; Gaming is opt-in everywhere. The power-key cycle
        // emits Turbo only when the model surface offers it, never Gaming.)
        // "Turbo" is the canonical Synapse name, sighted stock on a sibling
        // 02C7 (2026-07-14); this fork previously guessed "HyperBoost". On
        // the 16 the wire doubles as the 175 W cooling-pad state — the
        // model-aware refusal below is the single chokepoint every caller
        // funnels through (CLI, GUI, config restore, reapply).
        if mode == 7 && !(self.allow_experimental || self.turbo_is_stock()) {
            eprintln!("refusing profile 7 (Turbo) — stock on the Blade 18; this model needs the experimental opt-in (About page)");
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

    /// Fan tachometer, zone 1. Callers must treat a silent EC as unknown,
    /// never as a number — no mirror fallback.
    pub fn read_fan_rpm_from_ec(&mut self) -> Option<u16> {
        self.read_fan_tach(0x01)
    }

    pub fn set_logo_led_state(&mut self, mode: u8) -> bool {
        if !self.static_lighting {
            return true; // lighting scope disabled (see set_standard_effect)
        }
        // Synapse 4 write parity, measured on the lid-logo LED (USBPcap
        // 2026-07-20): EVERY mode click writes the effect first, then the
        // on/off state — including "off" (effect 0 + state 0). Both commands
        // use varstore 0x00 on this LED (NOT the 0x01 the keyboard paths
        // use — args are not uniform across functions). The state setter
        // 0x03/0x00 answers NOT_SUPPORTED on the 2025 EC — for Synapse's own
        // writes too — but the values still go into the same registers;
        // whether the EC honours them despite the status code is the open
        // probe question (H1/H2).
        let mut effect: RazerPacket = RazerPacket::new(0x03, 0x02, 0x03);
        effect.args[0] = 0x00; // varstore 0 on the logo LED (measured)
        effect.args[1] = RazerLaptop::LOGO_LED;
        effect.args[2] = if mode == 2 { 0x02 } else { 0x00 };
        let effect_ok = self.send_report(effect).is_some();

        let mut state: RazerPacket = RazerPacket::new(0x03, 0x00, 0x03);
        state.args[0] = 0x00; // varstore 0 (measured)
        state.args[1] = RazerLaptop::LOGO_LED;
        state.args[2] = self.clamp_u8(mode, 0x00, 0x01);
        // Result not gated: the known 0x05 must not fail the whole call while
        // the accepted effect write went through.
        let _ = self.send_report(state);

        effect_ok
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

    /// Live EC brightness read (0x03/0x83). Verified live on this unit
    /// (2026-07-22): answers the configured brightness. None on silence.
    pub fn get_brightness(&mut self) -> Option<u8> {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x83, 0x03);
        report.args[0] = RazerLaptop::VARSTORE;
        report.args[1] = RazerLaptop::BACKLIGHT_LED;
        report.args[2] = 0x00;
        self.send_report(report).map(|response| response.args[2])
    }

    /// Stock-feature flags come from laptops.json (`features`), not from a
    /// PID hardcode — adding a model is a data edit. The pid-keyed free
    /// helpers below remain ONLY as a test-enforced mirror for the pure
    /// validation layer (see stock_feature_helpers_mirror_laptops_json).
    pub fn turbo_is_stock(&self) -> bool {
        self.features.iter().any(|f| f == "turbo_stock")
    }
    pub fn max_tier_is_stock(&self) -> bool {
        self.features.iter().any(|f| f == "max_tier_stock")
    }

    pub fn get_bho(&mut self) -> Option<u8> {
        if !self.have_feature("bho".to_string()) {
            return None;
        }

        let mut report: RazerPacket = RazerPacket::new(0x07, 0x92, 0x01);
        // The fork's 0x92 request carries remaining=1, mirroring Synapse's
        // captured request. MEASURED (probe, 2026-07-22): the reply carries 1
        // even for a 0-request — reply-side remaining is METADATA here, not
        // an echo; expected_reply_remaining demands exactly that 1.
        report.remaining_packets = 0x0001;
        report.args[0] = 0x00;

        self.send_report(report)
            .map(|resp| resp.args[0])
    }

    pub fn set_bho(&mut self, is_on: bool, threshold: u8) -> bool {
        if !self.have_feature("bho".to_string()) {
            return false;
        }

        // Wire-parity note (operator ruling 2026-07-20: mirror Synapse on the
        // wire — documenting deltas is not enough): args[0] carries the VALUE
        // across this whole trio — the packed on/threshold byte on the setter,
        // zero on the status read and the commit. The design-decision-8
        // blanket "args[0] = profileId on class 0x07" does NOT apply here.
        let mut report = RazerPacket::new(0x07, 0x12, 0x01);
        report.args[0] = bho_to_byte(is_on, threshold);
        let ok = self.send_report(report).is_some_and(|r| {
            // Multi-line packet dump demoted: it fired on every BHO write
            // including the boot restore (journal-diet consistency).
            log::debug!("BHO response packet: {:?}", r);
            true
        });
        if !ok {
            return false;
        }

        // Write parity (operator ruling 2026-07-20: same values into the same
        // registers as Synapse — writes, not reads): Synapse 4 follows the
        // setter with a commit 0x0f carrying arg 0x00 (measured; supersedes
        // the earlier arg-2 reading). Mirrored as a write; its failure does
        // not demote the setter's success — the commit is redundant on the
        // 2025 EC for apply AND persist [V, §8]. Note args[0] semantics
        // differ per command on class 0x07: value on 0x12, zero here — the
        // decision-8 "args[0]=profileId" blanket does not apply.
        let commit = RazerPacket::new(0x07, 0x0f, 0x01);
        if self.send_report(commit).is_none() {
            eprintln!("BHO commit (0x0f): no reply — harmless on 2025 (redundant), logged for the record");
        }
        true
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
                // Mirror of the TX swap: without it the EC's big-endian
                // remaining (00 01) deserializes as 256 and the metadata
                // table rejects the reply as silence.
                buf.swap(3, 4);
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
                        // Log dedup ONLY — the write itself always goes out.
                        // Write parity means we put Synapse's values into
                        // Synapse's registers even when the EC answers 0x05
                        // (it does so for Synapse's own logo-state writes
                        // too); whether the EC honours them despite the
                        // status code is a separate, open question.
                        let key = (report.command_class, report.command_id);
                        if !self.unsupported_cmds.contains(&key) {
                            eprintln!(
                                "Command not supported (class {:#04x} id {:#04x}) — further rejects of this command log at debug level only; the write itself keeps being sent (wire parity)",
                                report.command_class, report.command_id
                            );
                            self.unsupported_cmds.push(key);
                        } else {
                            log::debug!(
                                "Command not supported (class {:#04x} id {:#04x})",
                                report.command_class, report.command_id
                            );
                        }
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


/// Expected reply-side `remaining_packets` per READ command — ONLY entries
/// verified on THIS tool's own wire belong here. Field regression 2026-07-20:
/// seeding the table from Synapse captures broke the charger read at the
/// barrel (fail-safe PD everywhere) — Synapse's replies carried 2/1 because
/// Synapse's REQUESTS carried 2/1: on our wire the EC ECHOES the request's
/// remaining (we send 0, replies carry 0). Cross-referencing foreign replies
/// against our requests was the error. 0x92 stays: the fork's own request
/// sends 1 and the reply carries 1 — consistent under both models, verified
/// live. Everything else is permissive until measured on OUR requests.
fn expected_reply_remaining(class: u8, id: u8) -> Option<u16> {
    match (class, id) {
        (0x07, 0x92) => Some(0x0001),
        _ => None,
    }
}
/// Classify a feature-report reply against the request that was just written.
/// Pure decision logic, separated from the HID I/O so it can be unit-tested.
fn classify_response(request: &RazerPacket, response: &RazerPacket) -> ResponseAction {
    // The lineage carried a BHO special case: replies to both the getter
    // (0x92) and the setter (0x12) were said to arrive with command id 0x92,
    // so any 0x92-shaped reply was accepted early — skipping class, remaining,
    // STATUS and the transaction id entirely. MEASURED on the 2025 EC
    // (BHO-DIAG, 2026-07-11): the setter's reply echoes its OWN id (0x12),
    // class, remaining and tid, with a clean SUCCESS status — exactly like
    // every other command. The special case was dead code for the setter and
    // a needless bypass for the getter; BHO runs the normal pipeline.
    if response.command_class != request.command_class
        || response.command_id != request.command_id
    {
        return ResponseAction::KeepPolling;
    }
    // WRITE replies echo the request's remaining_packets. READ-style replies
    // (command_id bit 7) use the field as reply METADATA instead — measured:
    // the fan-ID read carries the payload length 0x0003, the charger read
    // 0x07/0x8c carries its payload length 0x0002 (USBPcap 2026-07-20), and
    // the BHO GET was captured carrying 1 from Synapse. Requiring an echo
    // there rejects every valid GET reply with non-zero metadata — the
    // charger read failed on exactly this. GETs are disambiguated by
    // class + id + the transaction-id staleness guard below instead.
    let is_get = request.command_id & 0x80 != 0;
    if is_get {
        // v2.14.1 (CONTRACTS.md): READs are checked against the MEASURED
        // per-command metadata value instead of a blanket exemption; READs
        // without a measurement stay permissive (class + id + the TID guard
        // below still apply).
        if let Some(expected) = expected_reply_remaining(request.command_class, request.command_id) {
            if response.remaining_packets != expected {
                return ResponseAction::KeepPolling;
            }
        }
    } else if response.remaining_packets != request.remaining_packets {
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
fn byte_to_bho(u: u8) -> (bool, u8) {
    (u & (1 << 7) != 0, (u & 0b0111_1111))
}

fn bho_to_byte(is_on: bool, threshold: u8) -> u8 {
    if is_on {
        return threshold | 0b1000_0000;
    }
    threshold
}

/// ============ Per-model capability matrix (v2.13) ============
/// THE single source: the validator, the load/attach sanitizer, the laptop
/// gates, the power-key cycle and (via GetCapabilities) the GUI/CLI lists
/// all derive from here. Never duplicate it client-side.
///
/// Canonical Synapse names throughout: wire 7 = "Turbo", boost tier 3 =
/// "Max" — both sighted STOCK on a sibling Blade 18 (02C7) Synapse UI,
/// 2026-07-14; this fork previously guessed "HyperBoost"/"Boost". The
/// Turbo⇢wire-7 mapping on 02C7 is an inference [I], not a capture: the
/// fifth AC tile plus the full-envelope-without-pad rationale. If a capture
/// ever measures differently, flip turbo_is_stock — one line.
///
///   FULL surface (= experimental ON, every model):
///     AC {0 Balanced, 2 Performance, 5 Silent, 7 Turbo, 4 Custom,
///         1 Gaming (legacy)}   DC {6 Balanced, 3 Battery Saver}
///     boost tiers 0..=3 (3 = Max)
///   Stock per model:  02C7 additionally ships Turbo and Max;
///     02C6 [V own captures] and 02C5 [assumed = 02C6] ship neither.
///   DC never gains anything, opt-in or not (Synapse parity, both models).
///
/// Rejecting at the boundary — before anything persists — is what keeps the
/// boot restore from replaying garbage forever. Fan curves stay unvalidated
/// on purpose: the apply path clamps every RPM (see the fan_clamp tests) and
/// the transport caps the payload.
// 14/16 are exercised by the test matrix; production paths only ever compare
// against the 18, because base == everything-not-stock on the other models.
#[allow(dead_code)]
const PID_BLADE_14_2025: u16 = 0x02C5;
#[allow(dead_code)]
const PID_BLADE_16_2025: u16 = 0x02C6;
const PID_BLADE_18_2025: u16 = 0x02C7;

/// MIRROR of laptops.json `features` ("turbo_stock") for the pure
/// validation layer — the data file is authoritative; a guard test parses
/// the real file and fails on any drift.
fn turbo_is_stock(pid: u16) -> bool {
    pid == PID_BLADE_18_2025
}

/// MIRROR of laptops.json `features` ("max_tier_stock"); same guard test.
/// Independent product decision that happens to coincide with Turbo today.
fn max_tier_is_stock(pid: u16) -> bool {
    pid == PID_BLADE_18_2025
}

fn profile_allowed_with(turbo_stock: bool, plugged: bool, pwr: u8, experimental: bool) -> bool {
    match (pwr, plugged) {
        (0, true) | (2, true) | (4, true) | (5, true) => true,
        (3, false) | (6, false) => true,
        (7, true) => experimental || turbo_stock,
        (1, true) => experimental,
        _ => false,
    }
}

fn boost_tier_valid_with(max_stock: bool, boost: u8, experimental: bool) -> bool {
    boost <= 2 || (boost == 3 && (experimental || max_stock))
}

/// Wires the power-key cycle skips even when they are on the effective
/// surface. OPERATOR PREFERENCE of this fork, not a Synapse rule: a stray
/// press must never land on Custom (4) or the legacy Gaming wire (1); both
/// stay reachable through CLI and GUI.
const CYCLE_EXCLUDED_WIRES: [u8; 2] = [4, 1];

/// The effective AC/DC surface in display order. Gaming (1) sits last and is
/// GUI-only by policy; the power-key cycle additionally drops the
/// CYCLE_EXCLUDED_WIRES. `turbo_stock` comes from laptops.json features.
/// Keep the name<->wire map in sync with cli::ProfileArg.
fn effective_profiles(turbo_stock: bool, plugged: bool, experimental: bool) -> Vec<u8> {
    if !plugged {
        return vec![6, 3];
    }
    let mut wires = vec![0, 2, 5];
    if experimental || turbo_stock {
        wires.push(7);
    }
    wires.push(4);
    if experimental {
        wires.push(1);
    }
    wires
}

/// Warm-press window for the power key: a second press within this many ms of
/// the previous SUCCESSFUL press advances the cycle; any later press
/// (re-)applies the current domain's profile instead (operator design
/// 2026-07-20 — the first press is a confirm/re-assert/heal, only a quick
/// follow-up cycles).
const POWER_KEY_ADVANCE_WINDOW_MS: u64 = 3000;

/// The power domain the daemon applies, derived from the EC charger class
/// (`read_charger`) rather than the binary UPower AC/DC split. Synapse gates
/// three surfaces, not two: on USB-C PD only Balanced is selectable — measured
/// on-device 2026-07-20, PD forces wire 0 and never gets the boost re-assert
/// the barrel gets.
///
///   Barrel  -> AC surface  (config slot 1: the user's stored AC profile)
///   Battery -> DC surface  (config slot 0: the user's stored DC profile)
///   Pd      -> the single wire 0 (Balanced), nothing stored, nothing to pick
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargerDomain {
    Barrel,
    Pd,
    Battery,
}

/// What the fans SHOULD be doing under the current parameters. Raw
/// persisted values pass through untouched (`Manual` carries the stored
/// i32); consumers keep their own range guards (see set_config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FanTarget {
    Auto,
    Manual(i32),
    Curve,
}

/// DESIRED — the one evaluation of "what should hold right now".
///
/// FILLING CHAIN (nothing in here is invented by this function):
///   PowerConfig::new()/dc_default   -> defaults on a fresh or quarantined
///                                      config (cpu_boost 2)
///   validated user writes           -> persisted into the slots
///   sanitize_loaded_config          -> repairs hand-edited survivors
///   ==> Configuration (daemon.json) is the ONLY value source; this
///       function adds the DOMAIN RULES and nothing else:
///   - Battery evaluates the DC slot, Barrel the AC slot, wholesale.
///   - Pd forces wire 0, borrows brightness/logo/fan from the AC slot,
///     and never writes anything back (measured Synapse parity).
///   - boosts are Some ONLY for Custom (wire 4): outside it the bytes
///     never reach the EC (0d/02 vs 0d/07, measured 8:1) — the persist
///     side of the same theorem lives in set_power_mode_in_domain.
///   - bho is domain-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DesiredState {
    wire: u8,
    boosts: Option<(u8, u8)>,
    brightness: u8,
    logo: u8,
    fan: FanTarget,
    bho: (bool, u8),
}

fn desired_state(cfg: &config::Configuration, domain: ChargerDomain) -> DesiredState {
    // Pd borrows the AC slot for everything but the wire.
    let slot = match domain {
        ChargerDomain::Battery => &cfg.power[0],
        ChargerDomain::Barrel | ChargerDomain::Pd => &cfg.power[1],
    };
    let wire = match domain {
        ChargerDomain::Pd => 0,
        _ => slot.power_mode,
    };
    let boosts = (wire == 4).then_some((slot.cpu_boost, slot.gpu_boost));
    let fan = if slot.fan_curve.enabled {
        FanTarget::Curve
    } else if slot.fan_rpm == 0 {
        FanTarget::Auto
    } else {
        FanTarget::Manual(slot.fan_rpm)
    };
    DesiredState {
        wire,
        boosts,
        brightness: slot.brightness,
        logo: slot.logo_state,
        fan,
        bho: (cfg.bho_on, cfg.bho_threshold),
    }
}

impl ChargerDomain {
    /// Classify the raw ACTP byte from `read_charger`. Barrel is the only value
    /// that unlocks the full AC surface; 0x00 is battery/none; everything else
    /// is an accepted PD contract -> the single-wire PD surface. Unknown/future
    /// PD classes fall here too, fail-safe (they get Balanced, never the AC
    /// ladder that a <=100 W source cannot sustain).
    pub fn from_actp(actp: u8) -> Self {
        match actp {
            0x11 => ChargerDomain::Barrel,
            0x00 => ChargerDomain::Battery,
            _ => ChargerDomain::Pd,
        }
    }

    /// The config array index this domain restores/persists through, matching
    /// the existing AcState convention (1 = AC slot, 0 = DC slot). PD borrows
    /// the AC slot for lighting/brightness parity but never writes power_mode
    /// back into it (see apply_charger_domain).
    pub fn config_index(self) -> usize {
        match self {
            ChargerDomain::Barrel | ChargerDomain::Pd => 1,
            ChargerDomain::Battery => 0,
        }
    }

    /// Wire-safe mirror for the socket protocol.
    pub fn to_wire(self) -> crate::comms::ChargerDomainWire {
        match self {
            ChargerDomain::Barrel => crate::comms::ChargerDomainWire::Barrel,
            ChargerDomain::Pd => crate::comms::ChargerDomainWire::Pd,
            ChargerDomain::Battery => crate::comms::ChargerDomainWire::Battery,
        }
    }
}

fn validate_power_request(
    pid: u16,
    ac: usize,
    pwr: u8,
    cpu: u8,
    gpu: u8,
    experimental: bool,
) -> Result<(), &'static str> {
    if ac > 1 {
        return Err("ac index out of range");
    }
    let plugged = ac == 1;
    if !profile_allowed_with(turbo_is_stock(pid), plugged, pwr, experimental) {
        return Err(match (pwr, plugged) {
            (7, true) => "Turbo needs the experimental opt-in on this model",
            (1, true) => "profile needs the experimental opt-in",
            (0..=7, _) => "profile not offered in this power domain",
            _ => "unknown profile value",
        });
    }
    let max_stock = max_tier_is_stock(pid);
    if !boost_tier_valid_with(max_stock, cpu, experimental)
        || !boost_tier_valid_with(max_stock, gpu, experimental)
    {
        return Err("boost tier out of range (0..=2; the Max tier needs an unlock on this model)");
    }
    Ok(())
}

/// The BHO wire codec packs on/off into bit 7 (`bho_to_byte`), so a
/// threshold above 127 would silently FLIP the on-bit. Same range the CLI
/// enforces (50..=80).
fn bho_threshold_valid(threshold: u8) -> bool {
    (50..=80).contains(&threshold)
}

/// Value-level guard for LOADED configs. The request path rejects invalid
/// values before they persist, but a legacy or hand-edited daemon.json can
/// still carry them — and the boot restore plus every domain switch replay
/// stored state verbatim. Reset offenders to safe defaults, loudly; returns
/// true when anything changed so the caller can persist the repair once.
/// `pid: None` = the load-time UNION pass (no device attached yet): only
/// values invalid on EVERY model fall, so a stored Turbo/Max survives until
/// the attach pass (`enforce_model_law`) knows whether this model ships it.
/// fan_rpm and curves stay exempt for the same reason as in the validator.
fn sanitize_loaded_config(cfg: &mut config::Configuration, pid: Option<u16>) -> bool {
    let turbo_stock = pid.is_none_or(turbo_is_stock);
    let max_stock = pid.is_none_or(max_tier_is_stock);
    let experimental = cfg.experimental_profiles;
    let mut changed = false;
    for ac in 0..2usize {
        let domain = if ac == 1 { "AC" } else { "battery" };
        let domain_default: u8 = if ac == 1 { 0 } else { 6 };
        let p = &mut cfg.power[ac];
        if !profile_allowed_with(turbo_stock, ac == 1, p.power_mode, experimental) {
            eprintln!(
                "config sanitized: stored profile {} is not valid on {} — reset to {}",
                p.power_mode, domain, domain_default
            );
            p.power_mode = domain_default;
            changed = true;
        }
        if !boost_tier_valid_with(max_stock, p.cpu_boost, experimental) {
            eprintln!(
                "config sanitized: stored CPU boost {} out of range on {} — reset to 2",
                p.cpu_boost, domain
            );
            p.cpu_boost = 2;
            changed = true;
        }
        if !boost_tier_valid_with(max_stock, p.gpu_boost, experimental) {
            eprintln!(
                "config sanitized: stored GPU boost {} out of range on {} — reset to 2",
                p.gpu_boost, domain
            );
            p.gpu_boost = 2;
            changed = true;
        }
        if p.logo_state > 2 {
            eprintln!(
                "config sanitized: stored logo state {} out of range on {} — reset to 0",
                p.logo_state, domain
            );
            p.logo_state = 0;
            changed = true;
        }
    }
    if !bho_threshold_valid(cfg.bho_threshold) {
        eprintln!(
            "config sanitized: stored BHO threshold {} outside 50..=80 — reset to 80",
            cfg.bho_threshold
        );
        cfg.bho_threshold = 80;
        changed = true;
    }
    changed
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
    fn get_reply_with_metadata_remaining_is_accepted() {
        // remaining_packets is an ECHO on this wire (falsified-metadata note:
        // ec-protocol §2); this GET has no own-wire expectation entry, so any
        // reply value must pass — the guard here is class+id+status+TID:
        // the charger read 0x07/0x8c answers with its payload length (0x0002,
        // USBPcap 2026-07-20) while the request carries 0. The old echo check
        // rejected every such reply — this is the regression that made
        // `razer-cli read charger` fail with a perfectly healthy EC.
        let request = RazerPacket::new(0x07, 0x8c, 0x02);
        let mut response = reply(0x07, 0x8c, RazerPacket::RAZER_CMD_SUCCESSFUL);
        response.remaining_packets = 0x0002;
        assert_eq!(classify_response(&request, &response), ResponseAction::Accept);
    }

    #[test]
    fn write_reply_with_mismatched_remaining_keeps_polling() {
        // WRITE replies DO echo remaining_packets; a mismatch there is still a
        // stale buffered reply and must not be accepted.
        let request = RazerPacket::new(0x0d, 0x02, 0x04);
        let mut response = reply(0x0d, 0x02, RazerPacket::RAZER_CMD_SUCCESSFUL);
        response.remaining_packets = 0x0003;
        assert_eq!(
            classify_response(&request, &response),
            ResponseAction::KeepPolling
        );
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
        let mut response = reply(0x07, 0x92, RazerPacket::RAZER_CMD_SUCCESSFUL);
        // Measured: the real 0x92 reply carries remaining_packets = 1 as READ
        // metadata (ec-protocol §2) — the fixture models the hardware, and the
        // per-command check (v2.14.1) verifies exactly this value.
        response.remaining_packets = 0x0001;
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
    fn bho_set_reply_echoes_its_own_id() {
        // MEASURED (BHO-DIAG, 2026-07-11): the 2025 EC answers the setter
        // 0x07/0x12 with id 0x12 — refuting the inherited 0x92 legend.
        let request = RazerPacket::new(0x07, 0x12, 0x01);
        let response = reply(0x07, 0x12, RazerPacket::RAZER_CMD_SUCCESSFUL);
        assert_eq!(classify_response(&request, &response), ResponseAction::Accept);
    }

    #[test]
    fn stray_getter_reply_for_set_request_keeps_polling() {
        // A buffered GET reply (0x92) while polling for the SET must not be
        // taken as the set's answer — the old early-accept did exactly that.
        let request = RazerPacket::new(0x07, 0x12, 0x01);
        let response = reply(0x07, 0x92, RazerPacket::RAZER_CMD_SUCCESSFUL);
        assert_eq!(
            classify_response(&request, &response),
            ResponseAction::KeepPolling
        );
    }

    #[test]
    fn bho_failure_status_is_resent_not_accepted() {
        // The old early-accept skipped the status check entirely; a FAILURE
        // reply counted as a successful BHO write. Now it resends.
        let request = RazerPacket::new(0x07, 0x12, 0x01);
        let response = reply(0x07, 0x12, RazerPacket::RAZER_CMD_FAILURE);
        assert_eq!(classify_response(&request, &response), ResponseAction::Resend);
    }

    #[test]
    fn bho_busy_status_keeps_polling() {
        let request = RazerPacket::new(0x07, 0x92, 0x01);
        let response = reply(0x07, 0x92, RazerPacket::RAZER_CMD_BUSY);
        assert_eq!(
            classify_response(&request, &response),
            ResponseAction::KeepPolling
        );
    }

    #[test]
    fn bho_stale_tid_keeps_polling() {
        // No TID exemption for BHO anymore: the echo is measured, the
        // promoted guard applies here like everywhere else.
        let request = RazerPacket::new(0x07, 0x12, 0x01);
        let mut response = reply(0x07, 0x12, RazerPacket::RAZER_CMD_SUCCESSFUL);
        response.id = request.id.wrapping_add(1);
        assert_eq!(
            classify_response(&request, &response),
            ResponseAction::KeepPolling
        );
    }

    #[test]
    fn bho_reply_from_the_wrong_class_keeps_polling() {
        let request = RazerPacket::new(0x07, 0x12, 0x01);
        let response = reply(0x03, 0x12, RazerPacket::RAZER_CMD_SUCCESSFUL);
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

    const ALL_PIDS: [u16; 3] = [PID_BLADE_14_2025, PID_BLADE_16_2025, PID_BLADE_18_2025];
    const BASE_ONLY: [u16; 2] = [PID_BLADE_14_2025, PID_BLADE_16_2025];

    #[test]
    fn every_model_accepts_its_base_matrix() {
        for pid in ALL_PIDS {
            for pwr in [0u8, 2, 4, 5] {
                assert!(validate_power_request(pid, 1, pwr, 0, 0, false).is_ok());
            }
            for pwr in [3u8, 6] {
                assert!(validate_power_request(pid, 0, pwr, 0, 0, false).is_ok());
            }
        }
    }

    #[test]
    fn turbo_and_max_are_stock_on_the_18_only() {
        assert!(validate_power_request(PID_BLADE_18_2025, 1, 7, 0, 0, false).is_ok());
        assert!(validate_power_request(PID_BLADE_18_2025, 1, 4, 3, 3, false).is_ok());
        for pid in BASE_ONLY {
            assert_eq!(
                validate_power_request(pid, 1, 7, 0, 0, false).unwrap_err(),
                "Turbo needs the experimental opt-in on this model"
            );
            assert_eq!(
                validate_power_request(pid, 1, 4, 3, 0, false).unwrap_err(),
                "boost tier out of range (0..=2; the Max tier needs an unlock on this model)"
            );
        }
    }

    #[test]
    fn experimental_is_the_full_unlock_everywhere() {
        for pid in ALL_PIDS {
            assert!(validate_power_request(pid, 1, 1, 0, 0, true).is_ok());
            assert!(validate_power_request(pid, 1, 7, 0, 0, true).is_ok());
            assert!(validate_power_request(pid, 1, 4, 3, 3, true).is_ok());
        }
    }

    #[test]
    fn gaming_needs_the_optin_even_on_the_18() {
        assert_eq!(
            validate_power_request(PID_BLADE_18_2025, 1, 1, 0, 0, false).unwrap_err(),
            "profile needs the experimental opt-in"
        );
    }

    #[test]
    fn the_battery_domain_never_gains_anything() {
        for pid in ALL_PIDS {
            for pwr in [0u8, 2, 4, 5, 1, 7] {
                assert_eq!(
                    validate_power_request(pid, 0, pwr, 0, 0, true).unwrap_err(),
                    "profile not offered in this power domain"
                );
            }
            for pwr in [3u8, 6] {
                assert_eq!(
                    validate_power_request(pid, 1, pwr, 0, 0, true).unwrap_err(),
                    "profile not offered in this power domain"
                );
            }
        }
    }

    #[test]
    fn unknown_and_out_of_range_values_are_rejected_on_every_model() {
        for pid in ALL_PIDS {
            assert_eq!(
                validate_power_request(pid, 1, 8, 0, 0, true).unwrap_err(),
                "unknown profile value"
            );
            assert_eq!(
                validate_power_request(pid, 1, 255, 0, 0, true).unwrap_err(),
                "unknown profile value"
            );
            assert_eq!(
                validate_power_request(pid, 2, 0, 0, 0, true).unwrap_err(),
                "ac index out of range"
            );
            assert!(validate_power_request(pid, 1, 0, 4, 0, true).is_err());
            assert!(validate_power_request(pid, 1, 0, 0, 255, false).is_err());
        }
        assert!(bho_threshold_valid(50) && bho_threshold_valid(80));
        assert!(!bho_threshold_valid(49) && !bho_threshold_valid(81));
    }

    #[test]
    fn the_effective_surface_is_ordered_and_model_true() {
        assert_eq!(effective_profiles(true, true, false), vec![0, 2, 5, 7, 4]);
        assert_eq!(effective_profiles(false, true, false), vec![0, 2, 5, 4]);
        assert_eq!(effective_profiles(false, true, true), vec![0, 2, 5, 7, 4, 1]);
        assert_eq!(effective_profiles(true, true, true), vec![0, 2, 5, 7, 4, 1]);
        for turbo_stock in [false, true] {
            assert_eq!(effective_profiles(turbo_stock, false, true), vec![6, 3]);
        }
    }

    #[test]
    fn desired_state_encodes_the_domain_rules_once() {
        let mut cfg = config::Configuration::new();
        cfg.power[0].power_mode = 3;
        cfg.power[0].brightness = 102;
        cfg.power[1].power_mode = 5;
        cfg.power[1].cpu_boost = 2;
        cfg.power[1].gpu_boost = 1;
        cfg.power[1].brightness = 127;
        cfg.power[1].fan_rpm = 3000;
        cfg.bho_on = true;
        cfg.bho_threshold = 80;

        // Battery takes the DC slot, Barrel the AC slot — wholesale.
        let bat = desired_state(&cfg, ChargerDomain::Battery);
        assert_eq!((bat.wire, bat.brightness), (3, 102));
        let barrel = desired_state(&cfg, ChargerDomain::Barrel);
        assert_eq!((barrel.wire, barrel.brightness), (5, 127));
        assert_eq!(barrel.fan, FanTarget::Manual(3000));
        assert_eq!(barrel.bho, (true, 80));

        // Boosts exist only for Custom — a Silent slot carrying stored
        // Custom tuning yields None (the apply side of the A9 theorem).
        assert_eq!(barrel.boosts, None);
        cfg.power[1].power_mode = 4;
        assert_eq!(desired_state(&cfg, ChargerDomain::Barrel).boosts, Some((2, 1)));

        // Pd forces wire 0, drops boosts, borrows the AC aux values.
        let pd = desired_state(&cfg, ChargerDomain::Pd);
        assert_eq!((pd.wire, pd.boosts, pd.brightness), (0, None, 127));

        // Fan priority mirrors set_config: curve > manual > auto.
        cfg.power[1].fan_rpm = 0;
        assert_eq!(desired_state(&cfg, ChargerDomain::Barrel).fan, FanTarget::Auto);
        cfg.power[1].fan_curve.enabled = true;
        assert_eq!(desired_state(&cfg, ChargerDomain::Barrel).fan, FanTarget::Curve);
    }

    #[test]
    fn stock_feature_helpers_mirror_laptops_json() {
        // laptops.json is the AUTHORITATIVE source of stock features; the
        // pid-keyed helpers are a mirror for the pure validation layer.
        // Parsing the real data file makes any drift fail CI.
        let raw = include_str!("../../data/devices/laptops.json");
        let devices: serde_json::Value = serde_json::from_str(raw).unwrap();
        for d in devices.as_array().unwrap() {
            let pid = u16::from_str_radix(d["pid"].as_str().unwrap(), 16).unwrap();
            let feats: Vec<&str> = d["features"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap())
                .collect();
            assert_eq!(turbo_is_stock(pid), feats.contains(&"turbo_stock"), "turbo_stock drift {pid:#06x}");
            assert_eq!(max_tier_is_stock(pid), feats.contains(&"max_tier_stock"), "max_tier_stock drift {pid:#06x}");
        }
    }

    #[test]
    fn the_union_pass_keeps_maybe_stock_values_for_the_attach_pass() {
        // Load time, no device: a stored Turbo/Max might be legitimate on the
        // model that attaches later — only absolute garbage falls.
        let mut cfg = config::Configuration::new();
        cfg.power[1].power_mode = 7;
        cfg.power[1].cpu_boost = 3;
        assert!(!sanitize_loaded_config(&mut cfg, None));
        cfg.power[1].power_mode = 1; // Gaming is opt-in on EVERY model
        cfg.power[0].power_mode = 9; // absolute garbage
        cfg.bho_threshold = 200;
        assert!(sanitize_loaded_config(&mut cfg, None));
        assert_eq!(cfg.power[1].power_mode, 0);
        assert_eq!(cfg.power[0].power_mode, 6);
        assert_eq!(cfg.bho_threshold, 80);
        assert_eq!(cfg.power[1].cpu_boost, 3); // still standing for the attach pass
    }

    #[test]
    fn the_attach_pass_enforces_the_real_model() {
        let stored_turbo_max = || {
            let mut c = config::Configuration::new();
            c.power[1].power_mode = 7;
            c.power[1].cpu_boost = 3;
            c
        };
        // On the 18 these ARE the stock surface — untouched.
        let mut on_18 = stored_turbo_max();
        assert!(!sanitize_loaded_config(&mut on_18, Some(PID_BLADE_18_2025)));
        // On the 16 without the opt-in they fall, loudly.
        let mut cfg = stored_turbo_max();
        assert!(sanitize_loaded_config(&mut cfg, Some(PID_BLADE_16_2025)));
        assert_eq!(cfg.power[1].power_mode, 0);
        assert_eq!(cfg.power[1].cpu_boost, 2);
        assert!(!sanitize_loaded_config(&mut cfg, Some(PID_BLADE_16_2025)));
    }

    #[test]
    fn valid_config_passes_every_sanitizer_untouched() {
        for pid in ALL_PIDS {
            let mut cfg = config::Configuration::new();
            assert!(!sanitize_loaded_config(&mut cfg, Some(pid)));
            let mut exp = config::Configuration::new();
            exp.experimental_profiles = true;
            exp.power[1].power_mode = 7;
            exp.power[1].cpu_boost = 3;
            assert!(!sanitize_loaded_config(&mut exp, Some(pid)));
        }
    }
}

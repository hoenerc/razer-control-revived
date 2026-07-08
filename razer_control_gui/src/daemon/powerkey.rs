// Power-mode key: cycle performance profiles from the keyboard.
//
// The Blade 16 2025 power-mode key (fn-row) arrives as an evdev event with
// MSC_SCAN value 0x700d3 (HID usage page 0x07, usage 0xD3) followed by an
// EV_KEY press on KEY_UNKNOWN (240). KEY_UNKNOWN is ambiguous, so matching is
// done on the scancode, which is unique.
//
// Design constraints (deliberate):
//  * No new crates. Raw evdev reads via the already-present `libc` dependency;
//    the listener thread sleeps in poll(2) and consumes zero CPU until a key
//    event arrives. Devices are selected by their key-capability bitmap
//    declaring KEY_UNKNOWN (240) — provably set on the interface that emits
//    the power key. Reality check (journal, 2026-07): composite HID devices
//    declare KEY_UNKNOWN too — a Razer Orochi V2 contributes two extra nodes,
//    so 4 watched nodes on the reference machine is normal, not a bug. Extra
//    nodes cost one fd each and never emit the scancode.
//  * Profile changes go through the daemon's OWN Unix socket as a normal
//    SetPowerMode command — the same path the CLI and GUI use. That path
//    persists the new profile to the config file, so a later restore
//    (AC switch, resume, daemon restart) re-applies the cycled choice instead
//    of silently reverting it. Never write to the EC directly from here.
//  * Cycling is domain-aware and mirrors the exposed profile sets:
//      AC:      Balanced(0) -> Performance(2) -> Silent(5) -> Balanced ...
//      Battery: Balanced(6) -> Battery Saver(3) -> Balanced ...
//    Custom (4) is intentionally NOT in the cycle: entering it requires
//    boost values, and a stray key press must never land in a manually tuned
//    state. (A historical fan-runaway in Custom was reclassified 2026-07-08 as
//    stuck EC runtime state, cleared by cold boot — see docs/CONTRACTS.md §2;
//    the cycle exclusion stands on the independent ground above.) If Custom
//    (or anything unexpected) is active, the next press goes to the domain's
//    Balanced.
//  * Feedback is DE-agnostic: primary path is KDE Plasma's on-screen display
//    (session-bus service org.freedesktop.Notifications, object
//    /org/kde/osdService, interface org.kde.osdService, showText(icon, text));
//    where that service is absent (GNOME, XFCE, ...) a standard freedesktop
//    notification is sent instead, reusing one replaces_id so cycling
//    replaces the bubble rather than stacking.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::comms;

const POWER_KEY_SCANCODE: i32 = 0x700d3;

// input_event field types (linux/input.h)
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_MSC: u16 = 0x04;
const MSC_SCAN: u16 = 0x04;

/// struct input_event on 64-bit: struct timeval (16) + u16 type + u16 code + i32 value = 24 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

const EVENT_SIZE: usize = std::mem::size_of::<InputEvent>();

pub fn start_power_key_task() -> JoinHandle<()> {
    thread::spawn(|| {
        // Small startup delay so the daemon's own socket listener is up before
        // the first key press could try to connect to it.
        thread::sleep(Duration::from_secs(2));
        // Supervision loop: run_listener returns when it has no usable device
        // (all fds died — USB re-enumeration after a suspend cycle — or none
        // could be opened). The old behaviour was a silent thread death until
        // the next daemon restart; now we rescan every 10 s. The detailed
        // diagnosis is logged once, then a reminder every ~5 minutes so the
        // journal is not spammed while e.g. the input-group fix is pending.
        let mut attempts: u64 = 0;
        loop {
            run_listener(attempts == 0);
            attempts += 1;
            if attempts > 1 && attempts % 30 == 1 {
                eprintln!(
                    "powerkey: still no usable input device after {} rescans — retrying every 10 s",
                    attempts - 1
                );
            }
            thread::sleep(Duration::from_secs(10));
        }
    })
}

fn run_listener(verbose: bool) {
    let files = open_key_devices();
    if files.is_empty() {
        if verbose {
            eprintln!("powerkey: no openable input device declares KEY_UNKNOWN(240) — profile-cycle key disabled (see any open errors above; check `id -nG` contains `input`)");
        }
        return;
    }
    println!("powerkey: listening on {} input device(s) for scancode {:#x}", files.len(), POWER_KEY_SCANCODE);

    let mut files = files;
    let mut pollfds: Vec<libc::pollfd> = files
        .iter()
        .map(|f| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 })
        .collect();

    // Per-device "the current SYN frame contained our scancode" flag.
    let mut armed: Vec<bool> = vec![false; files.len()];
    let mut last_trigger: Option<Instant> = None;
    let mut buf = [0u8; EVENT_SIZE * 32];

    loop {
        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("powerkey: poll failed: {err} — listener stopping");
            return;
        }

        let mut dead: Vec<usize> = Vec::new();
        for i in 0..pollfds.len() {
            let re = pollfds[i].revents;
            if re & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                dead.push(i);
                continue;
            }
            if re & libc::POLLIN == 0 {
                continue;
            }
            let n = unsafe {
                libc::read(pollfds[i].fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                dead.push(i);
                continue;
            }
            let n = n as usize;
            for chunk in buf[..n].chunks_exact(EVENT_SIZE) {
                // Safety: chunk is exactly EVENT_SIZE bytes of a repr(C) POD read
                // from the kernel's input event stream.
                let ev: InputEvent = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) };
                match (ev.type_, ev.code) {
                    (EV_MSC, MSC_SCAN) if ev.value == POWER_KEY_SCANCODE => {
                        armed[i] = true;
                    }
                    (EV_KEY, _) if armed[i] && ev.value == 1 => {
                        // Key press belonging to our scancode's frame.
                        armed[i] = false;
                        let debounced = last_trigger
                            .map_or(true, |t| t.elapsed() >= Duration::from_millis(250));
                        if debounced {
                            last_trigger = Some(Instant::now());
                            cycle_profile();
                        }
                    }
                    (EV_SYN, _) => {
                        // Frame boundary: scancode announcements do not carry
                        // across SYN_REPORT.
                        armed[i] = false;
                    }
                    _ => {}
                }
            }
        }

        // Drop devices that vanished (unplugged); keep indices consistent.
        for &i in dead.iter().rev() {
            files.remove(i);
            pollfds.remove(i);
            armed.remove(i);
        }
        if pollfds.is_empty() {
            eprintln!("powerkey: all input devices gone — rescanning in 10 s");
            return;
        }
    }
}

/// KEY_UNKNOWN (240) — the keycode the power key maps to. Devices are
/// selected by whether their key-capability bitmap declares it: the kernel's
/// input core drops event codes a device has not declared, so the interface
/// that emitted the evtest-captured event provably has this bit set. This
/// picks exactly the fn-key interface(s) and skips mice/touchpads without the
/// coarse "has keys and no REL axes" heuristic — composite HID interfaces
/// (common on Razer keyboards) carry both and were wrongly excluded by that.
const KEY_UNKNOWN: usize = 240;

/// Test one bit in a sysfs capability bitmap ("%lx"-words, most-significant
/// word first, 64-bit words on x86_64).
fn key_bitmap_has(caps_key: &str, bit: usize) -> bool {
    let words: Vec<u64> = caps_key
        .split_whitespace()
        .filter_map(|w| u64::from_str_radix(w, 16).ok())
        .collect();
    let word_from_lsb = bit / 64;
    if words.len() <= word_from_lsb {
        return false;
    }
    let w = words[words.len() - 1 - word_from_lsb];
    (w >> (bit % 64)) & 1 == 1
}

/// Open every /dev/input/event* node whose key bitmap can emit KEY_UNKNOWN.
/// Every accepted device and every open failure is logged so the journal
/// shows exactly what is (or is not) being monitored.
fn open_key_devices() -> Vec<fs::File> {
    let mut out = Vec::new();
    let entries = match fs::read_dir("/dev/input") {
        Ok(e) => e,
        Err(e) => {
            eprintln!("powerkey: cannot list /dev/input: {e}");
            return out;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("event") {
            continue;
        }
        let caps_key = fs::read_to_string(format!("/sys/class/input/{}/device/capabilities/key", name))
            .unwrap_or_default();
        if !key_bitmap_has(&caps_key, KEY_UNKNOWN) {
            continue;
        }
        match fs::File::open(format!("/dev/input/{}", name)) {
            Ok(f) => {
                let dev_name = fs::read_to_string(format!("/sys/class/input/{}/device/name", name))
                    .unwrap_or_default();
                println!("powerkey: monitoring /dev/input/{} ({})", name, dev_name.trim());
                out.push(f);
            }
            Err(e) => {
                eprintln!("powerkey: cannot open /dev/input/{}: {e} (user not in `input` group?)", name);
            }
        }
    }
    out
}

/// Advance to the next profile in the current power domain, through the
/// daemon's own socket (persists to config), and show a KDE OSD with the name.
fn cycle_profile() {
    let ac = on_ac_power();
    let ac_idx: usize = if ac { 1 } else { 0 };

    // Exposed cycle per domain (wire values). Custom deliberately excluded.
    let order: &[u8] = if ac { &[0, 2, 5] } else { &[6, 3] };

    let current = match query_u8(comms::DaemonCommand::GetPwrLevel { ac: ac_idx }) {
        Some(v) => v,
        None => {
            eprintln!("powerkey: could not read current profile from daemon socket");
            return;
        }
    };
    // Preserve the stored Custom boost values: SetPowerMode writes cpu/gpu into
    // the config, so sending zeros here would erase the user's Custom setup.
    let cpu = query_u8(comms::DaemonCommand::GetCPUBoost { ac: ac_idx }).unwrap_or(0);
    let gpu = query_u8(comms::DaemonCommand::GetGPUBoost { ac: ac_idx }).unwrap_or(0);

    let next = match order.iter().position(|&v| v == current) {
        Some(pos) => order[(pos + 1) % order.len()],
        None => order[0], // Custom/ghost/unknown active -> go to domain Balanced
    };

    let ok = matches!(
        send(comms::DaemonCommand::SetPowerMode { ac: ac_idx, pwr: next, cpu, gpu }),
        Some(comms::DaemonResponse::SetPowerMode { result: true })
    );
    if !ok {
        eprintln!("powerkey: SetPowerMode({next}) via socket failed");
        return;
    }

    let name = match next {
        0 | 6 => "Balanced",
        2 => "Performance",
        5 => "Silent",
        3 => "Battery Saver",
        _ => "Unknown",
    };
    println!("powerkey: cycled to {name} (wire {next}, {})", if ac { "AC" } else { "battery" });
    show_kde_osd(name);
}

fn on_ac_power() -> bool {
    if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
        for entry in entries.flatten() {
            let p = entry.path();
            let is_mains = fs::read_to_string(p.join("type"))
                .map_or(false, |t| t.trim() == "Mains");
            if is_mains {
                if let Ok(online) = fs::read_to_string(p.join("online")) {
                    return online.trim() == "1";
                }
            }
        }
    }
    true // no supply info: assume AC (safe default on a laptop on the desk)
}

fn send(cmd: comms::DaemonCommand) -> Option<comms::DaemonResponse> {
    let sock = comms::bind()?;
    comms::send_to_daemon(cmd, sock)
}

fn query_u8(cmd: comms::DaemonCommand) -> Option<u8> {
    match send(cmd)? {
        comms::DaemonResponse::GetPwrLevel { pwr } => Some(pwr),
        comms::DaemonResponse::GetCPUBoost { cpu } => Some(cpu),
        comms::DaemonResponse::GetGPUBoost { gpu } => Some(gpu),
        _ => None,
    }
}

/// Profile-change feedback, DE-agnostic. Primary path: KDE Plasma's OSD (the
/// centered overlay used for volume / keyboard layout / Plasma's own power
/// profiles). If that service is absent (GNOME, XFCE, ...), fall back to a
/// standard freedesktop notification, which every desktop's notification
/// daemon implements. The fallback reuses one notification id (replaces_id)
/// so rapid cycling replaces the bubble instead of stacking new ones, and is
/// marked transient so it does not land in notification history.
fn show_kde_osd(text: &str) {
    let conn = match dbus::blocking::Connection::new_session() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("powerkey: no session bus for feedback: {e}");
            return;
        }
    };

    // 1. KDE OSD
    let proxy = conn.with_proxy(
        "org.freedesktop.Notifications",
        "/org/kde/osdService",
        Duration::from_millis(500),
    );
    let res: Result<(), dbus::Error> = proxy.method_call(
        "org.kde.osdService",
        "showText",
        ("preferences-system-power-management", text),
    );
    if res.is_ok() {
        return;
    }

    // 2. freedesktop notification fallback (any DE)
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    static LAST_NOTIFY_ID: AtomicU32 = AtomicU32::new(0);

    let proxy = conn.with_proxy(
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        Duration::from_millis(500),
    );
    let mut hints: HashMap<&str, dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>> = HashMap::new();
    hints.insert("transient", dbus::arg::Variant(Box::new(true)));
    let notify: Result<(u32,), dbus::Error> = proxy.method_call(
        "org.freedesktop.Notifications",
        "Notify",
        (
            "razer-control",                          // app_name
            LAST_NOTIFY_ID.load(Ordering::Relaxed),   // replaces_id (0 = new)
            "preferences-system-power-management",    // icon
            "Power profile",                          // summary
            text,                                     // body
            Vec::<&str>::new(),                       // actions
            hints,                                    // hints
            2000_i32,                                 // timeout ms
        ),
    );
    match notify {
        Ok((id,)) => LAST_NOTIFY_ID.store(id, Ordering::Relaxed),
        Err(e) => eprintln!("powerkey: no OSD service and notification failed: {e}"),
    }
}

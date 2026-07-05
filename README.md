# Razer Laptop Control — Revived · Blade 16 2025 personal fork

**This is a personal fork, purpose-built for one machine: the Razer Blade 16 (2025) on Linux.**
It exists because USB captures of Razer Synapse on Windows showed that the 2025 EC firmware
deviates substantially from what the upstream tools assume — most importantly, the power-profile
value map shipped by every fork in this lineage does not match what Synapse actually sends on
this generation. This fork re-bases the tool on the **measured** 2025 protocol and, as a
consequence, **supports 2025 models only**.

License: [GPL-2.0](LICENSE) — same as the entire lineage.

---

## Lineage & credits

This project stands on three layers of prior work, in order:

1. **Original project:** [Razer-Linux/razer-laptop-control-no-dkms](https://github.com/Razer-Linux/razer-laptop-control-no-dkms)
   — the userspace (no kernel module, no DKMS) Razer laptop control daemon/CLI this whole family derives from.
2. **Revival:** [encomjp/razer-control-revived](https://github.com/encomjp/razer-control-revived)
   — brought the project back to life: HID communication modifications, the GTK4/libadwaita GUI,
   packaging, and ongoing device support. See its README for that project's own contributor credits.
3. **Fork base:** [wsquarepa/razer-control-revived](https://github.com/wsquarepa/razer-control-revived)
   — the fork this repository is built on; its changes over encomjp are listed below.
4. **This fork:** [hoenerc/razer-control-revived](https://github.com/hoenerc/razer-control-revived)
   — Blade-16-2025-specific rework based on Windows Synapse USB captures and EC behavioural
   measurement, maintained for the author's own system. Issues and PRs about other models belong
   upstream.

---

## Supported devices

| Device | PID | Status |
|---|---|---|
| Razer Blade 16 2025 | `02C6` | **Verified** — profile map, boost semantics, fan range and event choreography measured against Windows Synapse USB captures |
| Razer Blade 14 2025 | `02C5` | Same EC generation, **assumed compatible, untested** (inherits upstream feature/fan data) |
| Razer Blade 18 2025 | `02C7` | Same EC generation, **assumed compatible, untested** (inherits upstream feature/fan data) |

**All pre-2025 models were removed** (device database and udev rule). The legacy profile value
map they rely on (`0=Balanced, 1=Gaming, 2=Creator, 3=Silent`) is exactly what this fork replaces,
so keeping them listed would have silently mis-programmed their ECs. For older hardware, use
[encomjp's](https://github.com/encomjp/razer-control-revived) or
[wsquarepa's](https://github.com/wsquarepa/razer-control-revived) fork.

---

## Why fork? The measurement story

Capturing Synapse's USB traffic on Windows (USBPcap, byte-level) against the Blade 16 2025 showed:

- The EC's **profile value map is generation-specific**: on 2025 firmware the wire values are
  `0=Balanced(AC) · 2=Performance · 3=Battery Saver · 4=Custom · 5=Silent · 6=Balanced(battery)`,
  with value `1` a non-functional legacy ghost and value `7` the 175 W cooling-pad HyperBoost
  state. The inherited map (`0..4`, linear) programs the wrong profiles on this hardware.
- The profile namespace is **partitioned by power domain**, not linear: Synapse offers
  Balanced/Performance/Silent/Custom on AC and Balanced/Battery Saver on battery, and "Balanced"
  is one logical profile with two domain-specific wire values (0 / 6).
- Command framing differs in detail: the boost/profile command class expects `args[0] = 0x01`.
- Several EC behaviours (warm-boot profile reset, GPU power-zone latching only while the dGPU is
  runtime-active, a Custom-mode fan-runaway firmware bug reproducible in Synapse itself) demanded
  daemon-side handling or explicit non-handling.

The full evidence-tagged protocol reference lives with the fork's patch documentation.

---

## Changelog vs. [encomjp/razer-control-revived](https://github.com/encomjp/razer-control-revived)

### Inherited from the fork base ([wsquarepa](https://github.com/wsquarepa/razer-control-revived))

- **Smart fan curves**: per-domain temperature→RPM curves with CPU / GPU / Both sources
  (Both = each temperature on its own curve, higher resulting RPM wins), config-persisted,
  CLI (`read/write fancurve`) and GUI curve editor.
- **dGPU power-zone re-latch machinery**: the custom-mode GPU boost/TGP only latches while the
  dGPU is runtime-active; a resume watcher re-applies the profile when the dGPU wakes, plus
  wake-settle re-applies after system resume (firmware resets the zone late).
- **Protocol hardening**: EC status codes (busy/failure/timeout) handled, confirmed writes
  (busy-poll until the EC acknowledges), report-CRC offset fix.
- **GUI**: sliders commit on release instead of flooding the daemon during a drag.
- CLI GPU boost extended to a 4th level (see below — disabled again for the Blade 16 2025 here).

### This fork (cumulative, v1 → v2.5)

**Profile system (the reason this fork exists)**
- Measured 2025 wire map with **real Synapse names** across daemon, CLI and GUI; CLI takes named
  profiles, GUI dropdown rebuilds **domain-aware** on the AC/Battery toggle.
- Wire 1 (legacy ghost) hidden everywhere; wire 7 (HyperBoost, 175 W cooling-pad state)
  **hard-blocked** at the single daemon chokepoint all callers pass through.
- `args[0]=0x01` on the profile/boost command class (Synapse parity); domain-correct config
  defaults (AC→0, battery→6); Custom boosts freely combinable (0–2 each, HIGH/HIGH allowed —
  it is budget allocation, not independent throttles).
- 4th CPU/GPU boost level disabled for the Blade 16 2025 via the device feature flag: the EC
  accepts value 3 but not Synapse-faithfully; untested by design.
- Fan range corrected to **2000–5100 RPM** (verified in Synapse UI; tool DB and third-party
  review both wrong).

**New: power-mode key**
- The fn-row power key (scancode `0x700d3`, matched on `MSC_SCAN` — the keycode is the ambiguous
  `KEY_UNKNOWN`) cycles profiles **domain-aware** with wrap-around; Custom is deliberately not in
  the cycle. Raw evdev via the existing `libc` dependency, blocking `poll(2)`, zero idle CPU;
  devices selected by their key-capability bitmap declaring KEY_UNKNOWN (provably set on the
  emitting interface). The switch goes through the daemon's **own socket** as a regular
  SetPowerMode, so it **persists to the config** and survives restore on resume/AC-switch/reboot.
  Feedback: KDE OSD, with a freedesktop-notification fallback on any other DE.

**Idle-power invariant & state-coupled monitoring**
- Baseline (fan mode auto/manual): **zero sensor reads, zero nvidia-smi** anywhere — GUI shows a
  battery/charge + fan line only. Smart-curve mode: the classic full monitor (CPU/iGPU/dGPU)
  returns, and the daemon's curve may read the dGPU temperature via nvidia-smi. Every dGPU access
  is gated on the dGPU being **runtime-active**, so a sleeping dGPU is never woken for display or
  curve input; GPU name resolution is lspci-only (kernel-cached PCI config, works in D3cold) with
  a process-lifetime cache.
- Known EC firmware bug, deliberately not worked around: with Custom active the EC runs the fans
  away past the manual range — **reproduced byte-identically in Windows Synapse**, so it is
  firmware, not tooling. Custom is left untouched until a firmware fix.

**Daemon steady-state & code health**
- dGPU sysfs path cached (was: a PCI directory scan every 2 s, forever); duplicate heavyweight
  path finder deleted (a second full scan per GUI poll); `envycontrol` availability cached (was:
  a process spawn every 2 s); keyboard animator skips all locking while no effects are active.
- `lazy_static` replaced by `std::sync::LazyLock`, unused `systemstat` dropped (−2 dependencies,
  no new toolchain requirement), dead code removed → warning-free build.

**GUI & scope**
- envycontrol section removed (distro guidance against it); monitoring reduced as above;
  "Check for Updates", the PayPal/donation surfaces, and the KDE plasmoid are removed from scope
  (the panel presence is the tray icon; the plasmoid was never installed by `install.sh`).
- About page reflects this fork: custom version string, links to both upstream repositories,
  "Tested on: Fedora & Arch Linux".

**Installer / portability**
- `install.sh`: `systemctl --user daemon-reload` before enable (fresh-install fix); binaries
  placed with `install -m755` (unlink-first — safe over a running old binary); `set -e` in the
  privileged blocks (no more silently half-applied installs); running GUI/tray stopped before
  replacement; uninstall completed (icon, data dir, unit reload) and reordered stop-first;
  `input`-group check with printed remedy for the power key. Re-running
  `./install.sh install` **is** the supported upgrade path; per-user config is preserved.
- Any-DE notes: tray = StatusNotifierItem (GNOME needs the standard AppIndicator extension;
  degradation is graceful), power-key feedback works everywhere via the notification fallback.

---

## Design decisions (binding for this fork)

- **Measured beats inherited**: every wire value, name, range and behaviour in the profile path
  comes from USB captures or on-device EC measurement, tagged by evidence class in the protocol
  reference. Where a third-party claim conflicted with a measurement, the measurement won.
- **Never emit what Synapse would not**: the ghost slot and HyperBoost are unreachable from every
  surface; the block sits at the one chokepoint rather than in each caller.
- **The sleeping dGPU is sacred**: no code path may wake a runtime-suspended dGPU for telemetry.
  nvidia-smi exists only behind the runtime-active guard, only in smart-curve mode (the NVIDIA
  driver exposes no hwmon node, so there is no sysfs alternative for GPU temperature).
- **State changes go through one door**: the power key is a socket client of its own daemon so
  persistence, restore and EC application share a single code path with CLI and GUI.
- **Minimal dependency surface**: no new crates for new features (evdev via `libc`, OSD via the
  existing `dbus`); dependencies were removed, not added.
- **Fail loud, degrade graceful**: installer aborts on first error; missing DE services (OSD,
  tray host) degrade with a log line, never a crash.
- **2025-only**: supporting the legacy map and the measured map in one tool means conditional
  protocol paths nobody here can test. Older machines are better served by upstream.

---

## Build & install

```
git clone https://github.com/hoenerc/razer-control-revived.git
cd razer-control-revived/razer_control_gui
cargo build --release
./install.sh install        # also the upgrade path; config is preserved
systemctl --user status razercontrol
```

Toolchain: Rust ≥ 1.85 (edition 2024). System libraries: gtk4, libadwaita, libdbus
(Fedora: `gtk4-devel libadwaita-devel dbus-devel` · Arch: `gtk4 libadwaita dbus` ·
Debian/Ubuntu: `libgtk-4-dev libadwaita-1-dev libdbus-1-dev`). hidapi uses the pure-Rust
`linux-native` backend. For the power-mode key the user must be in the `input` group
(the installer checks and prints the command if not).

---

## License

GPL-2.0, unchanged through the whole lineage. See [LICENSE](LICENSE).

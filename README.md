# Razer Laptop Control — Revived · Blade 16 2025 personal fork

![CI](https://github.com/hoenerc/razer-control-revived/actions/workflows/ci.yml/badge.svg)

**A personal fork, purpose-built for one machine: the Razer Blade 16 (2025) on Linux.**
USB captures of Razer Synapse on Windows showed that the 2025 EC firmware deviates substantially
from what every upstream tool in this lineage assumes — most importantly the power-profile wire
map. This fork re-bases the tool on the **measured** 2025 protocol and consequently supports
**2025 models only**. License: [GPL-2.0](LICENSE), unchanged through the whole lineage.

## Status

**v2.14 — feature-complete, maintenance mode.** The daemon validates every request against the
measured matrix before anything persists, state paths report the truth, and the test suite
covers the protocol decision logic (30 unit tests, blocking clippy, MSRV 1.85 in CI). From here:
keep CI green, rebase-verify on toolchain moves, change code only when a measurement says so.
History: [`CHANGELOG.md`](CHANGELOG.md) · decisions: [`docs/CONTRACTS.md`](docs/CONTRACTS.md).

## Features

- **Measured 2025 profile system** with real Synapse names, partitioned by power domain
  (AC: Balanced / Performance / Silent / Custom · battery: Balanced / Battery Saver).
- **Custom mode** with freely combinable CPU/GPU levels (budget allocation, HIGH/HIGH allowed).
- **Smart fan curves** per domain with CPU / GPU / Both temperature sources; the battery view
  offers no fan configuration — exactly like Synapse.
- **Power-mode key**: the fn-row key cycles profiles domain-aware with on-screen feedback;
  the choice persists across resume, AC switches and reboots.
- **Battery Health Optimizer** (charge limit), keyboard brightness, a single static keyboard
  colour, and logo control.
- **Lighting master switch**: turned off, the daemon performs zero lighting writes — colour,
  brightness, logo, suspend hooks — so OpenRazer & friends can own the hardware conflict-free.
- **Per-model surface** (v2.13): profiles and boost tiers follow what each model ships
  stock — the Blade 18 gets Turbo (wire 7) and the Max tier out of the box; canonical
  Synapse names everywhere. The **experimental opt-in** (About page, default off) is the
  full unlock on every model and additionally exposes Gaming (legacy, wire 1).
- **Charger-aware power domains** (v2.14): barrel, USB-PD and battery are told apart via
  the EC adapter class. Under USB-PD the tool pins Balanced (volatile — the stored AC
  choice survives), and the power key confirms on the first press and cycles on a quick
  second one (3 s window).
- **Hardened daemon**: requests are validated before they persist (the boot restore can only
  replay validated state), confirmed EC writes with a transaction-id staleness guard,
  crash-safe and self-sanitizing config persistence, request timeouts, and a gap-free
  `state change:` journal timeline. Zero-wake guarantee: a runtime-suspended dGPU is never
  woken for telemetry.

## Supported devices

| Device | PID | Status |
|---|---|---|
| Razer Blade 16 2025 | `02C6` | **Verified** — profile map, boost semantics, fan range and event choreography measured against Synapse USB captures |
| Razer Blade 14 2025 | `02C5` | Same EC generation, **assumed = Blade 16, untested** |
| Razer Blade 18 2025 | `02C7` | Profile surface & canonical names corroborated via a sibling Synapse UI (2026-07-14); **Turbo⇢wire-7 mapping inferred, not captured** |

Pre-2025 models were removed on purpose: the legacy wire map they need is exactly what this
fork replaces, and listing them would silently mis-program their ECs. For older hardware use
[encomjp's](https://github.com/encomjp/razer-control-revived) or
[wsquarepa's](https://github.com/wsquarepa/razer-control-revived) fork.

## Install

```
git clone https://github.com/hoenerc/razer-control-revived.git
cd razer-control-revived/razer_control_gui
./install.sh install        # builds release, installs, enables the user service; also the upgrade path
systemctl --user status razercontrol
```

Requirements: Rust ≥ 1.85 (edition 2024), systemd user session; system libraries gtk4,
libadwaita, libdbus (Fedora: `gtk4-devel libadwaita-devel dbus-devel` · Arch:
`gtk4 libadwaita dbus` · Debian/Ubuntu: `libgtk-4-dev libadwaita-1-dev libdbus-1-dev`).
hidapi uses the pure-Rust `linux-native` backend. The power key needs the user in the
`input` group — the installer checks and prints the remedy. Per-user configuration survives
upgrades and uninstall (`~/.local/share/razercontrol/`).

**Troubleshooting:** the unit stops retrying after repeated fast failures —
`systemctl --user reset-failed razercontrol` re-arms it. Verbose IPC logging:
`RAZER_LAPTOP_CONTROL_LOG=debug`. Device access is a per-seat ACL plus the `input` group;
`getfacl /dev/hidraw*` should list your user on the Razer nodes.

## Usage

GUI: `razer-settings` (closes to tray). CLI examples:

```
razer-cli write power ac silent      # named profiles, per domain (ac | bat)
razer-cli write fan ac 0             # 0 = automatic, otherwise RPM in the model range
razer-cli read gpu                   # GPU inventory + runtime-PM status (read-only)
razer-cli read charger               # raw EC adapter class: 0x11 barrel / 0x00 none / else USB-PD tier
razer-cli --help                     # full command surface incl. fan curves and BHO
```

The power key cycles the model's effective surface — AC `Balanced → Performance → Silent`
(plus `Turbo` where the model or the opt-in offers it) and battery
`Balanced → Battery Saver`; Custom and Gaming stay excluded —
a stray key press must never land in a manually tuned or legacy state.

On GNOME the feedback uses a bundled companion extension (installed and enabled by
`install.sh`; Wayland needs one re-login) that shows the native OSD pill above fullscreen
games — GNOME locks its own `ShowOSD` to gnome-settings-daemon callers, and plain
notifications stay hidden over fullscreen. Elsewhere: KDE's OSD, then a transient
notification.

## Design in brief

The long-form reasoning lives in the documentation below; the short version:

- **Measured beats inherited.** Every wire value, name and range comes from USBPcap captures
  or on-device measurement, evidence-tagged; conflicts resolve in favour of the measurement.
  The founding one: the 2025 wire map (`0/2/3/4/5/6`, domain-partitioned) is not the legacy
  linear map — inherited tools program the wrong profiles on this hardware.
- **Nothing persists that did not validate.** One daemon-side chokepoint mirrors the measured
  matrix; invalid requests leave no trace, and loaded configs are sanitized against the same
  rules — the boot restore can only replay validated state.
- **Lighting is deliberately small.** One static colour, brightness, logo — the per-key engine
  is gone (~2,200 lines), and the master switch means *zero* lighting writes when off, enforced
  in the daemon, so full-featured tools can own the hardware conflict-free.
- **The sleeping dGPU is sacred**, state changes go through one door (power key, CLI and GUI
  share the same daemon path), and the dependency surface only ever shrinks.

## Documentation

- [`docs/ec-protocol.md`](docs/ec-protocol.md) — the evidence-tagged EC protocol reference
  (wire map, framing, rejection semantics, power characterisation, reclassifications).
- [`docs/CONTRACTS.md`](docs/CONTRACTS.md) — binding design contracts, measurement provenance,
  do-not-fix list, and the dated decision appendices (§9 = the v2.11 finishing decisions).
- [`CHANGELOG.md`](CHANGELOG.md) — the full cumulative history (v1 → v2.11) including
  everything inherited from the fork base.

## Lineage & credits

1. **Original:** [Razer-Linux/razer-laptop-control-no-dkms](https://github.com/Razer-Linux/razer-laptop-control-no-dkms) — the userspace daemon/CLI this family derives from.
2. **Revival:** [encomjp/razer-control-revived](https://github.com/encomjp/razer-control-revived) — HID rework, GTK4/libadwaita GUI, packaging.
3. **Fork base:** [wsquarepa/razer-control-revived](https://github.com/wsquarepa/razer-control-revived) — fan curves, dGPU re-latch machinery, protocol hardening.
4. **This fork:** [hoenerc/razer-control-revived](https://github.com/hoenerc/razer-control-revived) — the measured 2025 rework, maintained for the author's own system. Issues about other models belong upstream.

## License

GPL-2.0 — see [LICENSE](LICENSE).

# Razer Laptop Control — Revived · Blade 16 2025 personal fork

![CI](https://github.com/hoenerc/razer-control-revived/actions/workflows/ci.yml/badge.svg)

**A personal fork, purpose-built for one machine: the Razer Blade 16 (2025) on Linux.**
USB captures of Razer Synapse on Windows showed that the 2025 EC firmware deviates substantially
from what every upstream tool in this lineage assumes — most importantly the power-profile wire
map. This fork re-bases the tool on the **measured** 2025 protocol and consequently supports
**2025 models only**. License: [GPL-2.0](LICENSE), unchanged through the whole lineage.

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
- **Experimental opt-in** (About page, default off): HyperBoost (wire 7), Gaming (legacy,
  wire 1, measured ≈ Performance alias) and a 4th CPU/GPU boost tier.
- **Hardened daemon**: confirmed EC writes with transaction-id staleness guard, crash-safe
  config persistence, request timeouts, boundary validation, and a gap-free `state change:`
  journal timeline. Zero-wake guarantee: a runtime-suspended dGPU is never woken for telemetry.

## Supported devices

| Device | PID | Status |
|---|---|---|
| Razer Blade 16 2025 | `02C6` | **Verified** — profile map, boost semantics, fan range and event choreography measured against Synapse USB captures |
| Razer Blade 14 2025 | `02C5` | Same EC generation, **assumed compatible, untested** |
| Razer Blade 18 2025 | `02C7` | Same EC generation, **assumed compatible, untested** |

All pre-2025 models were removed on purpose: the legacy wire map they need
(`0=Balanced, 1=Gaming, 2=Creator, 3=Silent`) is exactly what this fork replaces, and listing
them would silently mis-program their ECs. For older hardware use
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
razer-cli --help                     # full command surface incl. fan curves and BHO
```

The power key cycles AC `Balanced → Performance → Silent` and battery
`Balanced → Battery Saver`; Custom and experimental profiles are deliberately excluded —
a stray key press must never land in a manually tuned state.

## Why this fork exists

Byte-level USBPcap captures of Synapse 4 against the Blade 16 2025 established a
generation-specific wire map (`0/2/3/4/5/6`, domain-partitioned; ghost slot 1; HyperBoost 7),
`args[0]=0x01` command framing, warm-boot profile reset, GPU power-zone latching only while the
dGPU is runtime-active, and more. The inherited linear map (`0..4`) programs the wrong profiles
on this hardware — that finding is the fork's founding measurement.

## Design: lighting is deliberately small

Since v2.10 the lighting scope is exactly one static colour, brightness and the logo — no
effects, no per-key engine, no animation loop. Two decisions make that a feature rather than
a limitation:

- **Static only.** The 2025 keyboard outgrew the per-key geometry this lineage inherited
  (ec-protocol §22), and a calm, single-colour keyboard is what this fork is for. Removing
  the effect machinery deleted ~2,200 lines the daemon no longer has to get right.
- **A master switch instead of ambition.** "Static keyboard lighting" off means the daemon
  performs *zero* lighting writes — colour, brightness, logo, suspend hooks — enforced at one
  chokepoint inside the daemon, not merely greyed out in the GUI. Want effects? Run OpenRazer
  or any full-featured tool alongside; nothing here will fight it.

The project's scope gets narrower without narrowing the user: power profiles, fan curves and
battery care are first-class either way.

## Design contracts (binding)

- **Measured beats inherited** — every wire value, name and range comes from captures or
  on-device measurement, evidence-tagged; conflicts resolve in favour of the measurement.
- **Never emit what Synapse would not** — ghost slot and HyperBoost sit behind one explicit,
  daemon-enforced opt-in; the power-key cycle never emits them; a single chokepoint gates all
  callers.
- **The sleeping dGPU is sacred** — nvidia-smi has exactly one call site (the daemon's curve
  task), multi-gated; the GUI only ever reads the daemon's cached snapshot.
- **State changes go through one door** — power key, CLI and GUI all funnel through the same
  daemon path; persistence, restore and EC application never diverge.
- **Minimal dependency surface** — features are built on existing crates; dependencies get
  removed, not added.
- **2025-only** — dual protocol paths nobody here can test would be worse than a clear scope.

## Documentation

- [`docs/ec-protocol.md`](docs/ec-protocol.md) — the evidence-tagged EC protocol reference
  (wire map, framing, rejection semantics, power characterisation, reclassifications).
- [`docs/CONTRACTS.md`](docs/CONTRACTS.md) — binding design contracts, measurement provenance,
  do-not-fix list, and the dated reconciliation appendix.
- [`CHANGELOG.md`](CHANGELOG.md) — the full cumulative history (v1 → v2.9) including everything
  inherited from the fork base.

## Lineage & credits

1. **Original:** [Razer-Linux/razer-laptop-control-no-dkms](https://github.com/Razer-Linux/razer-laptop-control-no-dkms) — the userspace daemon/CLI this family derives from.
2. **Revival:** [encomjp/razer-control-revived](https://github.com/encomjp/razer-control-revived) — HID rework, GTK4/libadwaita GUI, packaging.
3. **Fork base:** [wsquarepa/razer-control-revived](https://github.com/wsquarepa/razer-control-revived) — fan curves, dGPU re-latch machinery, protocol hardening.
4. **This fork:** [hoenerc/razer-control-revived](https://github.com/hoenerc/razer-control-revived) — the measured 2025 rework, maintained for the author's own system. Issues about other models belong upstream.

## License

GPL-2.0 — see [LICENSE](LICENSE).

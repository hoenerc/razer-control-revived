# Changelog — hoenerc/razer-control-revived

Cumulative, narrative-style history of this fork. Newer structural documentation lives in
`docs/CONTRACTS.md` (binding design contracts) and `docs/ec-protocol.md` (measured EC protocol).
Release tags: `v2.6`, `v2.7`, `v2.8`, `v2.9` — `git log <tag>..<tag>` gives the per-release view.

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

### This fork (cumulative, v1 → v2.9)

**Profile system (the reason this fork exists)**
- Measured 2025 wire map with **real Synapse names** across daemon, CLI and GUI; CLI takes named
  profiles, GUI dropdown rebuilds **domain-aware** on the AC/Battery toggle.
- Wire 1 (legacy ghost) hidden everywhere; wire 7 (HyperBoost, 175 W cooling-pad state)
  **hard-blocked** at the single daemon chokepoint all callers pass through.
  *(since v2.9: both reachable behind the explicit experimental opt-in; the chokepoint and the
  power-key exclusion stand unchanged.)*
- `args[0]=0x01` on the profile/boost command class (Synapse parity); domain-correct config
  defaults (AC→0, battery→6); Custom boosts freely combinable (0–2 each, HIGH/HIGH allowed —
  it is budget allocation, not independent throttles).
- 4th CPU/GPU boost level disabled for the Blade 16 2025 via the device feature flag: the EC
  accepts value 3 but not Synapse-faithfully; untested by design.
  *(since v2.9: the feature flag is gone; tier 3 sits behind the experimental opt-in for CPU and
  GPU alike.)*
- Fan range corrected to **2000–5100 RPM** (verified in Synapse UI; tool DB and third-party
  review both wrong).

**Protocol refinements from fresh AC/DC Synapse captures (v2.7)**
- **Transaction id cycles 1..=30** (was 0..=30): across a 35-frame capture the ids run 0x01–0x1e
  globally monotonic and wrap 0x1e → 0x01 — Synapse never emits 0x00 or 0x1f, so neither do we.
- **Custom-entry choreography corrected to the measured wire**: exactly four writes, no reads —
  zone 1 profile, zone 2 profile, CPU boost, GPU boost. The lineage-inherited read-before-write
  pattern is gone (its read results were discarded anyway); four HID round trips saved per
  Custom apply, including every dGPU-resume re-latch burst. Retired getters stay in-tree as
  annotated EC diagnostic reads for measurement sessions.
- **TID staleness guard (promoted)**: the EC echoes the request's transaction id — proven live
  by a full day of polling traffic (thousands of accepted replies, zero `TID mismatch` journal
  lines; a non-echoing EC would have logged on every accept). Accepted-looking replies carrying
  the wrong id are now treated as the previous command's buffered reply and polled past, closing
  the back-to-back zone1/zone2 race in the confirmed-write handshake. The BHO special path
  (0x92 oddball) stays log-only until a deliberate toggle session confirms its echo too.
- **Undervolt decoded**: Synapse's undervolt toggle has NO separate EC command on this
  interface — it grays the Custom CPU selector and pins the wire to CPU boost High
  (`0d 07 01 01 02`); disabling restores the previously selected value. The undervolting
  proper happens host-side. Implication on Linux with firmware undervolt active: keep Custom
  CPU boost at High to stay Synapse-faithful.
- Still pending measurement before mirroring: the leading CPU-boost write Synapse fires on
  every AC/DC domain switch (suspected re-assert of the stored Custom CPU boost), and a
  bidirectional capture to confirm the TID echo directly.

**Robustness batch (v2.7)**
- **Crash-safe config writes**: daemon.json / effects.json go tmp-in-same-dir + fsync + atomic
  rename. A crash mid-write used to truncate the config, and the next start silently reset
  everything — BHO off, curves gone, custom boosts gone.
- **Socket hardening**: 2 s read/write timeouts and a 64 KiB request cap on the daemon's accept
  path — a client that connected and never finished could previously park the single-threaded
  command loop (power key included) forever.
- **Fan-RPM boundary validation**: `SetFanSpeed` rejects values outside 0/model-range instead of
  the old unchecked i32→u16 cast (−1 wrapped to max fans, 70000 to a silently wrong speed).
- **Power-key supervision**: the listener rescans /dev/input every 10 s after losing all devices
  (USB re-enumeration on a suspend cycle) instead of dying silently until the next daemon
  restart.
- **hwmon path cached** for the per-tick CPU temperature read (auto-rescan if the cached node
  vanishes), matching the existing dGPU sysfs path cache.
- **Journal diet**: per-request REQ/RES lines demoted to debug level (measured ~55k journal
  lines per day from GUI polling); `RAZER_LAPTOP_CONTROL_LOG=debug` re-enables them.
- **GUI timers gated on visibility**: both 2 s pollers skip their tick while their widgets are
  unmapped (window in tray, page not shown) — measured ~27k daemon requests in half a day from
  exactly this. Tradeoff: the tray tooltip freezes at the last visible values while hidden.
- **State changes are journal events**: every Set command logs one info line (`state change:`),
  so powerkey cycles, profile reapplies AND GUI/CLI-initiated switches form one gap-free
  timeline; the 2 s Get polling stays at debug level.
- **Uninstall symmetry**: stops a running GUI/tray, `udevadm trigger`s after rule removal so
  hidraw nodes reset immediately, and prints how to leave the `input` group if desired.

**Experimental profiles — opt-in unlock (v2.9)**
- New About-page toggle (default OFF, persisted, enforced daemon-side) exposing what the EC
  accepts but Synapse never offers on the Blade 16: the **Gaming (legacy, wire 1)** and
  **HyperBoost (wire 7)** profiles in the AC list, plus a **Boost tier (value 3)** for the
  CPU/GPU levels in Custom. Wire 7 is Blade-18-native and cooling-pad-only on the 16, hence the
  mode-7 chokepoint and the tier caps now key on the toggle instead of refusing unconditionally.
  Compare with Synapse on Windows; use at your own risk. Deliberately NOT part of the power-key
  cycle (unknown modes keep falling back to Balanced). The `"boost"` capability flag left
  laptops.json — it encoded the wrong concept (a lineage CPU tier) on unverified courtesy
  entries; per user verification only the Blade 18 has Boost, and there it is profile 7.
- **Measurement corpus committed**: `docs/ec-protocol.md` (the full evidence-tagged EC protocol
  reference, v2 incl. the §21 runaway reclassification) and `docs/CONTRACTS.md` (binding design
  contracts + provenance from the original measurement sessions, with a dated reconciliation
  appendix) now live in-repo — no future session depends on chat memory.
- **Protocol arithmetic under test**: transaction-id advance (1..=30 wrap, capture-derived),
  fan clamp (range snap + 100-RPM truncation), curve lookup (documented ceiling semantics:
  strictly-greater step, floor below the first point) and the BHO byte codec (incl. the
  open-razerkit 0x41 cross-check) — pure functions, zero-cost coverage for the fresh CI.

**Scope cut (v2.8): GPU mode switching and the dGPU-suspend toggle removed**
- envycontrol integration (mode query/switch, `razer-cli write gpu-mode`) and the "Suspend dGPU"
  control (`write runtime-pm`, GUI switch) are gone. Both actuators required root writes
  (`envycontrol -s`, `power/control`) that a user daemon cannot perform — the controls never
  functioned on this install — and runtime-PM policy belongs to the distro's udev rules, where
  D3cold demonstrably works without our help. Read-only GPU status stays: `razer-cli read gpu`
  (name, driver, PCI slot, runtime status, PM policy).
- IPC note: this removes mid-enum variants and shifts bincode's variant indices — a coordinated
  break, valid because install.sh rebuilds and redeploys daemon + CLI + GUI together. Routine
  protocol evolution remains append-only.
- **Socket pinned to 0600** (owner-only): the inherited `umask(0o000)` dance — a relic of the
  root-daemon era when "non-root GUI/CLI" had to reach a root daemon's socket — is gone. Daemon
  and clients are the same user; the explicit mode mainly closes the formerly world-writable
  /tmp fallback.
- **CI**: a minimal GitHub Actions workflow builds and tests every push (`cargo build`,
  `cargo test`, both `--locked`); clippy runs informationally until the existing lint debt is
  measured, then flips to blocking. Replaces nothing — the upstream release pipeline was removed
  with the artifact sweep — and exists because three blind-edit compile errors in one session
  made the argument better than any principle could.
- **Battery tab offers no fan configuration (Synapse parity, user-verified)**: Synapse 4 exposes
  neither manual RPM nor curve editing on battery, so the GUI's Cooling section hides in the DC
  view. The capability stays in daemon and CLI (`razer-cli write fan bat ...`) as an escape
  hatch — the EC accepts DC zone writes (Synapse performs them on every DC profile switch); the
  constraint is a UI-layer rule in Synapse and is mirrored at the same layer here.

**New: power-mode key**
- The fn-row power key (scancode `0x700d3`, matched on `MSC_SCAN` — the keycode is the ambiguous
  `KEY_UNKNOWN`) cycles profiles **domain-aware** with wrap-around; Custom is deliberately not in
  the cycle. Raw evdev via the existing `libc` dependency, blocking `poll(2)`, zero idle CPU;
  devices selected by their key-capability bitmap declaring KEY_UNKNOWN (provably set on the
  emitting interface). The switch goes through the daemon's **own socket** as a regular
  SetPowerMode, so it **persists to the config** and survives restore on resume/AC-switch/reboot.
  Feedback: KDE OSD, with a freedesktop-notification fallback on any other DE.

**Idle-power invariant & state-coupled monitoring** *(architecture tightened in v2.6)*
- Baseline (fan mode auto/manual): **zero sensor reads, zero nvidia-smi** anywhere — GUI shows a
  battery/charge + fan line only. Smart-curve mode: the classic full monitor (CPU/iGPU/dGPU)
  returns. Every dGPU access is gated on the dGPU being **runtime-active**, so a sleeping dGPU is
  never woken for display or curve input; GPU name resolution is lspci-only (kernel-cached PCI
  config, works in D3cold) with a process-lifetime cache.
- **v2.6 — daemon-led dGPU telemetry, one nvidia-smi call site project-wide**: the GUI no longer
  runs nvidia-smi at all. The daemon's curve task makes **one** combined call per tick
  (`temperature.gpu,power.draw,utilization.gpu` — power/util ride along at zero extra GSP-RPC
  cost), and only while a smart curve with a **GPU/Both source** is enabled *and* the dGPU is
  already awake. The snapshot is cached with a timestamp; the GUI reads it via the
  `GetDgpuSensors` IPC command and hides the dGPU row when nothing fresh (≤ 10 s) exists —
  including with a CPU-only curve. A GUI parked in the tray therefore cannot generate GPU/driver
  traffic during gameplay. Verified live via execve tracing: only `razer-daemon` ever spawns
  nvidia-smi, at curve cadence, stopping the moment the curve is disabled.
- Documented tradeoff, accepted by design: while a **GPU/Both** curve is enabled and the dGPU is
  awake, the 2 s sampling keeps resetting the runtime-PM autosuspend timer, so the dGPU will not
  re-enter D3cold until the curve is disabled or its source set to **Cpu**. If post-game dGPU
  sleep matters more than GPU-temperature-driven fans, use a CPU-source curve.
- Custom-mode fan runaway — **reclassified (2026-07-08)**: originally graded a firmware bug
  because Windows Synapse reproduced it byte-identically; it vanished after a cold boot with no
  firmware update involved. Verdict: persistent stuck EC *runtime state* (plausibly induced by
  early raw probing), cleared by a power cycle — the Synapse cross-check silently assumed a
  stateless EC and was contaminated by shared state. Custom is fully usable; the power-key cycle
  keeps excluding it on independent grounds (boost values required). If it ever recurs: capture
  fan RPM + the preceding command log FIRST, then switch profiles and cold-boot. Details:
  `docs/CONTRACTS.md` §2, `docs/ec-protocol.md` §21.

**Daemon steady-state & code health**
- dGPU sysfs path cached (was: a PCI directory scan every 2 s, forever); duplicate heavyweight
  path finder deleted (a second full scan per GUI poll); `envycontrol` availability cached (was:
  a process spawn every 2 s); keyboard animator skips all locking while no effects are active.
- `lazy_static` replaced by `std::sync::LazyLock`, unused `systemstat` dropped (−2 dependencies,
  no new toolchain requirement), dead code removed → warning-free build.
- **Build modernized (v2.6)**: the stabilized `edition2024` cargo-feature flag dropped (was a
  warning on ≥ 1.85 toolchains), `rust-version = "1.85"` declared, unused `rand` dependency
  removed (−5 transitive crates), release profile set to ThinLTO + single codegen unit + symbol
  strip. `bincode` is pinned at `=1.3.3` **by design**: it encodes the byte-exact 91-byte
  RazerPacket EC framing and the daemon IPC; bincode ≥ 2 defaults to variable-length integer
  encoding, so a casual upgrade would silently corrupt the EC protocol.

**GUI & scope**
- envycontrol section removed (distro guidance against it); monitoring reduced as above;
  "Check for Updates", the PayPal/donation surfaces, and the KDE plasmoid are removed from scope
  (the panel presence is the tray icon; the plasmoid was never installed by `install.sh`).
- About page reflects this fork: version single-sourced from `Cargo.toml` (`CARGO_PKG_VERSION`
  at build time — GUI About, `razer-cli --version` and package version cannot diverge), links to
  both upstream repositories, "Tested on: Fedora & Arch Linux".

**Installer / portability**
- `install.sh`: `systemctl --user daemon-reload` before enable (fresh-install fix); binaries
  placed with `install -m755` (unlink-first — safe over a running old binary); `set -e` in the
  privileged blocks (no more silently half-applied installs); running GUI/tray stopped before
  replacement; uninstall completed (icon, data dir, unit reload) and reordered stop-first;
  `input`-group check with printed remedy for the power key. Re-running
  `./install.sh install` **is** the supported upgrade path; per-user config is preserved.
- **udev rule fixed & renamed (v2.6)**: `99-hidraw-permissions.rules` → `70-razercontrol.rules`.
  `TAG+="uaccess"` is processed by `73-seat-late.rules`, so at `99-*` the tag was inert (which is
  why per-seat ACLs never appeared) and world-writable `MODE="0666"` silently carried all access
  for every local process. The rule now ships `MODE="0660", GROUP="input"` (the daemon user is in
  `input` anyway — power-key requirement) plus a **working** uaccess per-seat ACL; once verified
  live via `getfacl /dev/hidraw*`, MODE/GROUP can optionally be dropped for a pure-ACL rule. The
  installer removes the legacy `99-*` file on upgrade and runs `udevadm trigger`, so a first
  install works without reboot/replug; the systemd unit gains a start-rate limit so a broken
  install fails loud instead of restart-looping every 5 s forever.
- Any-DE notes: tray = StatusNotifierItem (GNOME needs the standard AppIndicator extension;
  degradation is graceful), power-key feedback works everywhere via the notification fallback.

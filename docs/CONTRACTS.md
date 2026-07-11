# Handover: measurement provenance & binding contracts
## hoenerc/razer-control-revived — from the protocol-measurement instance

**Purpose.** This document transfers what the original measurement/design sessions established
and what is NOT fully visible from the repository alone: evidence provenance, binding design
contracts, and a do-not-fix list. It complements (does not compete with) the v2.6–v2.8 audit
work already done in your session (70- udev prefix, daemon-led dGPU telemetry, TID echo check,
socket permission remediation — all acknowledged as correct and superseding where they overlap).

**Recommended action: commit this file as `docs/CONTRACTS.md`** (and the full EC protocol
reference alongside it as `docs/ec-protocol.md`) so no future instance depends on chat memory.

---

## 1. Measured 2025 EC protocol — the facts everything rests on

Source: byte-level USBPcap captures of Windows Synapse against the Blade 16 2025 (1532:02C6),
plus on-device EC measurement. Evidence classes: [V] operator-verified/measured, [X] third-party
claim overridden by measurement.

- **Profile wire map (generation-specific):**
  `0 = Balanced (AC) · 2 = Performance · 3 = Battery Saver · 4 = Custom · 5 = Silent · 6 = Balanced (battery)`.
  `1` = non-functional legacy ghost (hidden everywhere). `7` = HyperBoost / 175 W cooling-pad
  state (hard-blocked at the single device.rs chokepoint — never emit it). [V]
- **Domain partition, not a linear list:** AC exposes {0,2,5,4}; battery exposes {6,3}.
  "Balanced" is one logical profile with two domain wire values (0/6). [V]
- **Command framing:** the profile/boost command class expects `args[0] = 0x01` (Synapse parity). [V]
- **Reads are config, not EC:** GetPwrLevel / GetCPUBoost / GetGPUBoost return the *stored config*
  values (get_ac_config), not live EC reads. Any logic that assumes an EC readback is wrong. [V]
- **Warm-boot reset:** the EC re-initialises the profile to wire 2 on warm boot → the daemon's
  restore-on-start is load-bearing, not cosmetic. [V]
- **Custom boost semantics:** CPU/GPU boost 0–2, freely combinable incl. HIGH/HIGH — it is power
  *budget allocation*, not independent throttles. The 4th level ("Boost", value 3): the EC accepts
  it but Synapse never sends it on this device → disabled via the laptops.json feature flag for
  02C6, untested by design. [V]
- **Custom GPU TGP latches only while the dGPU is runtime-active**; the resume-watcher/re-apply
  machinery (inherited from wsquarepa, since refined) exists because of this. [V]
- **Fan range 02C6: 2000–5100 RPM** — verified in the Synapse UI. Overrides both the inherited
  tool DB (2200–5000) and a published review (1900–5300). Provenance chain: operator measurement
  beats review beats inherited data. Applies to 02C6 only. [V/X]

## 2. Custom-mode fan runaway — RECLASSIFIED (2026-07-08)

**History:** with profile 4 active the fans ran away past the manual range; reproduced
byte-identically by the tool AND by Windows Synapse → originally classified "EC firmware bug,
frozen, no workaround".

**New observation (operator, 2026-07-08):** the runaway is gone — no EC/firmware update
involved; it disappeared after a cold boot.

**Reclassification:** persistent stuck EC *runtime state* (plausibly induced during the early
raw reverse-engineering probing), cleared by a power cycle — **not** a firmware logic defect.
The decisive earlier evidence, the Synapse cross-check, carried a silent statelessness
assumption: a stuck EC state reproduces on any host, so that test was contaminated by shared
state and no longer proves anything about firmware logic. Whether the cold boot performed a
full EC reset or merely a boot-path re-init of the fan controller is unproven — do not
overclaim the mechanism. [V for the observations; mechanism open]

**Consequences:**
- Custom is **usable again** via GUI/CLI. The earlier interpretation "in Custom the EC runs no
  internal curve; the host must regulate" is **withdrawn** (it was derived from the runaway).
- The **power-key cycle keeps excluding Custom** — that decision had an independent ground that
  still stands: entering Custom requires boost values, and a stray keypress must never land in a
  manually-tuned state.
- If the runaway ever recurs: capture fan RPM + the exact preceding command log FIRST, then
  switch the profile away and cold-boot (known remedy). Treat it as state corruption evidence.
- Stale code comments still calling it a "firmware bug" (powerkey.rs cycle comment, possibly
  monitor comments) should be updated in the next code round; repo README section "Known EC
  firmware bug" should be replaced with this reclassification.

## 3. Binding architecture contracts

- **One door for state changes:** every profile write goes through
  `DeviceManager::set_power_mode` (config write + EC apply). The power key is deliberately a
  *socket client of its own daemon* sending a regular SetPowerMode — that is what makes the
  cycled choice persist across restore (resume / AC switch / reboot). Never bypass to the EC.
- **Wire values are the socket contract.** The bincode DaemonCommand protocol carries wire
  values; profile *names* exist only in CLI/GUI presentation. The daemon never calls the CLI.
- **Power key specifics:** trigger = scancode `0x700d3` matched on `MSC_SCAN` within the SYN
  frame (`KEY_UNKNOWN`/240 is ambiguous, never match on it). Device selection = key-capability
  bitmap declaring bit 240 (the input core drops undeclared codes, so the emitting interface
  provably carries the bit; an earlier `key!=0 ∧ rel==0` heuristic failed on composite Razer HID
  interfaces). Cycle: AC `0→2→5`, battery `6→3`, wrap-around; unknown/Custom active → domain
  Balanced; never emits 1/4/7. Stored Custom boosts are read (GetCPUBoost/GetGPUBoost) and passed
  through so cycling never clobbers them. Feedback: KDE OSD primary
  (`org.kde.osdService.showText`), freedesktop-notification fallback with reused `replaces_id` +
  transient hint on any other DE.
- **Zero-wake invariant:** no code path may wake a runtime-suspended dGPU for telemetry.
  Root fact: **the NVIDIA driver (open modules included) exposes no hwmon node** — verified
  against current sources incl. NVIDIA's own forum (open feature request); a v2 attempt to read
  GPU temp via hwmon was silently inert and was reverted. Therefore nvidia-smi is the only GPU
  temperature source and must stay multi-gated: smart-curve enabled ∧ GPU/Both source ∧ dGPU
  runtime-active (sysfs check first — nvidia-smi itself wakes a sleeping GPU). GPU *name*
  resolution is lspci-only (kernel-cached PCI config, works in D3cold) with a process-lifetime
  cache. Your session's move to daemon-led telemetry + visibility gating extends this invariant;
  keep the runtime-active pre-check wherever telemetry lives.
- **State-coupled monitoring intent:** baseline (fan mode auto/manual) = battery/charge + fan
  line only, zero sensor reads anywhere. Full CPU/iGPU/dGPU metrics exist only in smart-curve
  mode (and, per your refinement, only while actually visible).
- **GTK closure discipline:** glib objects are refcounted — clone handles before `move` closures
  that are also used afterwards (a v2.2 E0382 came from toggling CSS on `main_box` inside the
  monitor timer). The build sandbox cannot compile gtk4/dbus/edition-2024 code; pure logic gets
  assertion harnesses, everything else is verified by the local `cargo build` (expected
  warning-free since dead-code removal).

- **Lighting master switch = zero writes (v2.10).** With `static_lighting` off the daemon
  performs no keyboard-lighting write of any kind — colour, brightness, logo, suspend hooks.
  Enforced at the laptop-level primitives (`set_standard_effect` / `set_brightness` /
  `set_logo_led_state`) behind a config-mirrored device flag, so every caller passes the same
  chokepoint; GUI greying is cosmetic on top. Same caliber as the zero-wake guarantee.
- **The one lighting write is a double write (v2.10).** The legacy 0x03/0x0a effect command is
  deliberately sent twice per apply: the 2025 EC renders it one command behind (ec-protocol
  §23). Do not "optimise" the second write away.
- **Nothing persists that did not validate (v2.11).** The measured profile matrix is enforced
  at the daemon boundary in the order validate → gate → persist → apply/defer, and loaded
  configs are sanitized against the same matrix (repair persisted once). The boot restore can
  therefore only ever replay validated state. New setters must keep this order.
- **State toggles are transactional; value writes are persist-first (v2.11).** A toggle
  returning false means "nothing changed" (the GUI snaps its switch back on that); a value
  write may persist what the EC refused this instant (colour). These are different truth
  semantics on purpose — do not unify them.
- **Re-assert paths report the truth (v2.11).** `reassert_fan_curve` mirrors the tick's
  failure discipline (un-established + dropped target on failure, retry next tick) and every
  call site logs; failures deliberately do NOT flow into request results.

## 4. Deliberate non-goals — do not "fix"

- Custom mode: usable again (see §2 reclassification); only the power-key cycle exclusion remains, on independent grounds.
- Fan-curve task ticking every 2 s while disabled: reviewed, kept (one uncontended mutex probe).
- Per-keypress D-Bus session connection for OSD: reviewed, kept (human-rate events).
- udev `MODE="0666"` on hidraw: originally kept because the install script's **OpenRC path has
  no logind, so uaccess ACLs do nothing there**. If your 70- rename tightened MODE, re-check the
  OpenRC story or drop OpenRC support explicitly — either is fine, but decide it consciously.
- envycontrol: UI removed; any remaining protocol fields were kept for compat (your session
  removed dead code — fine, supersedes).
- `get_bho` (hardware BHO read, `#[allow(dead_code)]`, no callers): its request carries
  remaining_packets=0 while the measured reply carries remaining_packets=1 (ec-protocol §3.4),
  so the v2.11 plain pipeline would reject the exchange. Do not "fix" this blindly in dormant
  code — set the request's remaining to 1 and verify on hardware IF it is ever wired up.

## 5. Device policy

2025-only, fully data-driven (verified: no model-specific code branches exist). laptops.json =
3 entries: 02C5 (Blade 14 2025), 02C6 (Blade 16 2025), 02C7 (Blade 18 2025). **Only 02C6 is
measurement-verified**; 14/18 carry inherited feature/fan data, assumed-compatible/untested —
never present their values as verified. Rationale for the cut: the legacy map older models need
(`0=Balanced,1=Gaming,2=Creator,3=Silent`) is exactly what this fork replaced; listing them would
silently mis-program their ECs.

## 6. Lineage & sync doctrine (measured 2026-07-05)

Chain: Razer-Linux/razer-laptop-control-no-dkms → encomjp → wsquarepa (+12 commits: fan curves,
dGPU re-latch, HID handshake hardening, slider fixes, GPU Extreme level) → hoenerc. Shared git
ancestry verified (merge-base exists); **encomjp was 0 commits ahead of wsquarepa** at
measurement time — nothing was missing downstream. Sync doctrine: remotes for encomjp+wsquarepa;
**cherry-pick specific daemon/EC fixes as the default**; rare full merges accepted with known
conflict surface (the fork deliberately deleted things upstream still edits); **never rebase the
published main**. After the protocol rework, wholesale upstream merges are effectively history —
cherry-pick is the realistic mode.

## 7. Operator constants (project-critical subset)

Verdict first; source-verify before asserting (search for anything version-sensitive); measured
beats inherited; minimal/reversible/auditable changes; no new crate dependencies, no AUR;
German conversation, English artifacts/code; verify after every step; he builds locally
(Rust ≥1.85 required by edition 2024; machine has 1.96) and reports results — never claim a
build outcome the sandbox cannot produce.

---

## 8. Reconciliation appendix — audit-session instance, 2026-07-08

The document above is committed verbatim as received. Deltas against the repo state at commit
time (v2.9 work in progress), so no future instance mistakes historical lines for current policy:

- **§1 boost tier / laptops.json flag — superseded by v2.9.** The `"boost"` capability flag is
  gone (it encoded a lineage CPU-tier concept on unverified courtesy entries). Tier 3 and the
  wire 1/7 profiles now sit behind an explicit opt-in ("experimental profiles", About page,
  default OFF, persisted, enforced daemon-side at the same chokepoint/caps). The power-key cycle
  still never emits 1/4/7 — the contract holds.
- **Wire 1 identity reconciled.** ec-protocol §5/§6 measured ghost slot 1 ≈ Performance alias
  (Δ0.1 W under CPU load; GPU preference unproven). The experimental dropdown therefore labels
  it as a measured alias, not as a distinct profile.
- **§4 udev/OpenRC — conscious decision pending.** The 70- rename shipped
  `MODE="0660", GROUP="input"` + uaccess: the OpenRC path REMAINS functional through the group
  (no logind required), so nothing broke. The planned stage 2 (pure per-seat ACL) is blocked on
  an explicit operator decision: drop OpenRC support, or keep 0660+input as the permanent state.
- **BHO commit (design decision 9 vs ec-protocol §8):** the code deliberately omits the commit
  `0x07/0x0f` — §8 measured it redundant on 2025 (setter alone applies + persists +
  BIOS-visible), independently cross-confirmed in this session via open-razerkit (the 2024
  model needs it) and the operator's BIOS persistence observation. Effect-parity over
  byte-parity, documented deviation.
- **Design decision 10 (fan curves dormant) — superseded by operator practice.** Smart curves
  are in active use since 2026-07-07; the battery view hides fan configuration entirely
  (Synapse parity, user-verified).
- **§2 comment debt cleared** in the same commit: the powerkey.rs cycle comment and both README
  mentions no longer call the runaway a firmware bug; the reclassification text replaced them.
- Everything else in this document matches the repo or is acknowledged as historical provenance.

## 9. v2.11 finishing decisions — closing instance, 2026-07-11

v2.11 is the fork's finishing release; from here the project is in **maintenance mode** (keep
CI green, rebase-verify on kernel/toolchain moves, fix only what a measurement demonstrates).
Items evaluated and deliberately NOT built, with rationale — do not resurrect without new
evidence:

- **ApplyOutcome / Deferred in the IPC result**: bool suffices — powerkey targets only the
  active domain (Deferred unreachable there) and already matches on `result: true`; the GUI
  treats stored-for-the-other-domain as success by design. A tri-state would be a coordinated
  break with no carrying use case.
- **GetPowerState snapshot command**: polling aesthetics, not correctness; three cheap reads
  on a 0600 local socket.
- **Golden-HID vectors / mock transport**: the classify tests already cover the decision
  logic; a mock transport layer is exactly the kind of machinery this fork removes.
- **Fan hysteresis, undervolt pin, 0x88 tacho probe, dGPU wake-source hunt**: elective
  measurements with no observed problem behind them.
- **OpenRC support**: §4/§8 document the blocker (no logind → uaccess ACLs dead); systemd
  user session is the supported environment.
- **USBPcap double-session** stays the ONE elective window: it would replace the contractual
  double write (§3) with a native command and could map the 2025 per-key matrix — but the
  double write costs single-digit milliseconds and is protected, so this may simply never
  happen.
- **BHO TID promotion**: resolved by deletion — the special path is gone, the promoted guard
  applies, and the setter's TID echo is measured (BHO-DIAG 2026-07-11).

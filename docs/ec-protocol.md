# Razer Blade 16 (2025) — EC Power / Fan / Battery Protocol Reference

**Device:** Razer Blade 16 2025, AMD Ryzen AI 9 HX 370 + NVIDIA RTX 5090 Laptop
**Control interface:** USB `1532:02c6`, hidraw, HID interface 2
**Firmware baseline:** BIOS 2.02, EC/MCU/VBIOS current as of 2026-07-04 (updated *before* all measurements)
**Host:** CachyOS, kernel `7.1.2-3-cachyos-razer`
**Document version:** v2 — 2026-07-04
**Companion tools:** `razer_probe.py` (our probe), `power_bench2.sh` (our harness), `razer-control-revived` fork `wsquarepa` (patch base)

**Method:** USBPcap capture of Razer Synapse 4 (V4.0.86.2606231034) on Windows for wire ground truth; direct hidraw probing on Linux for behaviour and rejection semantics; RAPL + tach + battery instrumentation for power characterisation. Every captured/probed packet CRC-verified programmatically (100+ packets, 0 failures).

**Evidence tags:** **[V]** verified (measured or read from source, reproduced), **[O]** observed (seen, mechanism not fully controlled), **[H]** hypothesis (plausible, untested), **[X]** external (third-party review).

---

## 0. Design decisions (operator: Christopher) — binding for the patch

These are deliberate scope choices, not technical limits. The patch implements Synapse behaviour faithfully and nothing beyond it.

1. **Strictly Synapse-faithful.** The tool mirrors what Synapse does on the wire and in policy; no "expert" values that Synapse never sends.
2. **AC profile set:** Balanced (0), Performance (2), Silent (5), Custom (4). Offered **only on AC**.
3. **DC profile set:** Balanced-DC (6), Battery Saver (3). Offered **only on DC**. Full Synapse route on battery.
4. **No cross-domain exposure.** AC values are not selectable on DC and vice versa, exactly as Synapse gates them by power source.
5. **Custom boost is unrestricted: CPU and GPU each 0/1/2, all combinations allowed including HIGH/HIGH.** Rationale: HIGH/HIGH is not an invalid state — under the shared budget the GPU wins and the result is functionally identical to Performance (2), i.e. redundant, not dangerous. The EC accepts any in-range value (Test A). Synapse itself permits GPU=HIGH alongside a locked CPU=HIGH once undervolt is active (undervolt frees CPU-side budget), so an unconditional lockout would be *stricter* than Synapse and break parity; a state-dependent lockout would need an undervolt check. Neither is built. Custom simply passes the boost values through. Custom reframed: the sliders are a *budget allocation* ("what do I sacrifice for the other"), not independent throttles.
6. **Ghost slot 1** (legacy "Gaming"): not exposed. Synapse does not surface it on this model.
7. **Slot 7 (HyperBoost / cooling-pad, 175 W): declared UNSAFE, never exposed, never set.** No cooling pad present; operating a pad-designed power envelope without the pad is out of scope on a €6000 machine.
8. **args[0] = 0x01 on all class-0x0d/0x07 commands** (Synapse parity; see §10).
9. **BHO commit step retained** (Synapse parity; harmless — see §8).
10. **Fan curve (fork feature) left dormant.** Operator does not use userspace fan-control loops; EC-native per-mode curves only.

---

## 1. Transport

| Item | Value | Tag |
|---|---|---|
| Control device | `1532:02c6`, HID **interface 2** | V |
| Write | EP0 control, `bmRequestType 0x21`, `bRequest 0x09` (SET_REPORT), `wValue 0x0300` (feature, report id 0), `wLength 90` | V |
| Read | Same 90-byte report with **command_id \| 0x80** via SET_REPORT, answer fetched via GET_REPORT | V |
| HID buffer | 91 bytes: `[0]` report id `0x00`, `[1..90]` the 90-byte Razer report | V |
| USBPcap framing | 28-byte pseudoheader + 8-byte setup + 90-byte report = 126-byte write frames; 118-byte responses (no setup) | V |

**Wireshark filters (USBPcap):** all Razer writes `usb.transfer_type == 0x02 && frame.len > 60`; power/fan class only `frame[42:1] == 0d` (write frames; responses shift offsets by −8).

## 2. Report format (90 bytes)

| Report off | Frame off (write) | Field | Notes | Tag |
|---|---|---|---|---|
| 0 | 36 | status | host `0x00` NEW; reply `0x02` SUCCESS, `0x05` NOT_SUPPORTED, `0x01`/`0x00`/`0x04` busy | V |
| 1 | 37 | transaction_id | rolling **0..30**; 31 is Synapse's reset boundary, never sent. Not validated by EC (tool's constant `0x1F` also works) | V |
| 2–3 | 38–39 | remaining_packets | `0x0000` for all except BHO GET (`0x0001`) | V |
| 4 | 40 | protocol_type | `0x00` | V |
| 5 | 41 | data_size | used arg bytes (profile 4, boost 3, BHO 1) | V |
| 6 | 42 | command_class | `0x0d` power/fan, `0x07` battery, `0x03` lighting | V |
| 7 | 43 | command_id | bit 7 set = GET variant | V |
| 8–87 | 44–123 | args[0..79] | per-command | V |
| 88 | 124 | **crc** | **XOR of report bytes [2..88)** | V |
| 89 | 125 | reserved | `0x00` | V |

**EC does not validate the CRC.** The base tool historically sent `crc = 0x00` (serialisation bug, §13) and the EC accepted every command. [O]

## 3. Commands

### 3.1 SET performance profile — `0x0d / 0x02`, data_size 4
```
args[0] = profileId : 0x01 (Synapse) / 0x00 (tool). INERT on this command (§10).
args[1] = zone      : 0x01, then 0x02 (Synapse always writes both)
args[2] = profile value (§4)
args[3] = fan flag  : 0x00 auto (all captures) / 0x01 manual
```
GET variant `0x0d/0x82`. Synapse does not read before plain profile switches. [V]

### 3.2 SET boost preset — `0x0d / 0x07`, data_size 3  (Custom only)
```
args[0] = profileId : 0x01 (Synapse) / 0x00 (tool)
args[1] = 0x01 CPU, 0x02 GPU
args[2] = preset : 0 = low, 1 = medium, 2 = high
```
- GET variant `0x0d/0x87`: Synapse reads **before every manual CPU preset write** (read-before-write), never for GPU, never for programmatic writes. [V]
- **HIGH/HIGH allowed.** Not a firmware constraint: the EC accepts it, and under the shared budget it collapses to Performance (GPU wins). Synapse permits GPU=HIGH with CPU pinned HIGH once undervolt is active. No client-side lockout (design decision 5). The sliders are a budget-allocation choice, not independent throttles. [V]
- **Boost value 3** ("boost", feature-gated in legacy tool): does **not** exist on this model (scale is 0/1/2). Dead code. [V]
- **Undervolt coupling:** enabling Synapse's CPU Voltage Optimizer fires `0x0d/0x07` CPU = **2 (HIGH)** and locks the slider; disabling restores the stored preset (observed: LOW). Undervolt pins CPU to HIGH. [V]

### 3.3 Fan — class `0x0d`
| Cmd | Purpose | Notes | Tag |
|---|---|---|---|
| `0x0d/0x02` (args `[pid, zone, mode, manualflag]`) | set zone fan mode | `mode` (args[2]) must equal the active profile value — EC keys manual/auto per profile slot | V (fork src) |
| `0x0d/0x01` (args `[pid, zone, rpm/100]`) | set zone RPM | manual RPM range **2000–5100 (operator-verified in Synapse UI)** — overrides review claim 1900–5300 [X] and tool DB 2200–5000; stored setpoint seen = 4500 | V |
| `0x0d/0x81` | read stored setpoint | ×100 encoding | V |
| `0x0d/0x88` | **read tachometer** | ×100 encoding; **real tach** (values creep, not target). Works on 2025. | V |

### 3.4 Battery Health Optimizer — class `0x07`
| Cmd | Purpose | Notes | Tag |
|---|---|---|---|
| `0x07/0x12` (arg = `pct\|0x80` on / `pct` off) | set charge limit | applies immediately, both directions | V |
| `0x07/0x92` (GET) | read limit | reply returns as id `0x92` with **remaining_packets=1** (only such command) | V |
| `0x07/0x0f` (arg `0x02`) | commit/apply | **redundant on 2025** for apply+persist (§8) | V |
| `0x07/0x8f` | status read | Synapse reads between set and commit | V/X |

## 4. Profile value map

Single 3-bit field, **0–7 valid, 8+ → NOT_SUPPORTED** (n=2). Rejected writes leave the register untouched (clean reject semantics). [V]

| Value | Meaning | Domain | Exposed by patch | Tag |
|---|---|---|---|---|
| 0 | **Balanced** (AC slot) | AC | yes | V |
| 1 | ghost — legacy "Gaming" | AC? | **no** | V (exists) / H (identity) |
| 2 | **Performance** | AC | yes | V |
| 3 | **Battery Saver** (new; needs current EC) | DC | yes (DC only) | V |
| 4 | **Custom** | AC | yes | V |
| 5 | **Silent** | AC | yes | V |
| 6 | **Balanced** (DC slot) | DC | yes (DC only) | V |
| 7 | **HyperBoost / cooling-pad (175 W)** | AC + pad | **NO — UNSAFE** | V (exists) / X (pad=175W) |

**Naming note:** 0 and 6 are the *same logical profile* (Balanced) selected by power domain — not two different profiles. Likewise the map is a domain-partitioned namespace, not a linear list. Legacy tool labels (`0=Balanced,1=Gaming,2=Creator,3=Silent,4=Custom`) do **not** match this device; slot semantics stayed adjacent across generations (2: Creator→Performance, 3: Silent→Battery Saver), which preserved rough backward compatibility without any version negotiation.

**EC does not enforce domain.** Writing an AC value on DC (or vice versa) returns SUCCESS and is stored — Synapse's AC/DC split is pure software policy, not a firmware safety check. [V, Test A]

## 5. Behavioural characterisation

### 5.1 AC — CPU limits (stress-ng, RAPL package, fixed OS state, BHO on/Not-charging)

Sustained = last 60 s of a 150 s load; fast = first ~20 s plateau.

| Slot | idle W | fast W | peak W | **sustained W** | Tctl °C | fan1/2 rpm | identity |
|---|---|---|---|---|---|---|---|
| 5 / 3 / 6 | 3.4 | ~35 | ~48 | **34.9 / 35.1 / 34.6** | ~63 | ~2.1–2.3k | shared 35 W "quiet" tier |
| 0 | 3.5 | 51 | 63 | **45.6** | 69 | 2.6k | Balanced mid-tier |
| 2 / 1 | 3.8 | ~77 | 93 / 91 | **74.9 / 74.8** | ~86 | 4.2–4.3k | **ghost 1 = Performance alias** (Δ0.1 W) |
| 7 | 3.7 | 85 | **97.4** | **79.9** | 87.3 | 4.6k | own tier > Performance — **UNSAFE, one-time** |

Notes:
- **Four distinct CPU tiers** — the profile communicates real limits, not generic defaults. [V]
- **STAPM is skin-temperature-aware (2D limit):** a cold chassis yields ~85 W sustained for Performance, a pre-warmed chassis ~75 W. Both correct; the limit is f(profile × skin state). [V] Cross-confirmed by review: CPU limit runs lower when GPU loaded / thin-chassis balancing. [X]
- **~30 s sawtooth** in every load (e.g. 77.5↔70) = SMU slow-limit window enforcement (budget fills → clamp → refill → burst). The 60 s mean integrates over it. [V]
- Idle package is profile-agnostic (~3.4 W); the earlier "Performance idle fan floor" was **thermal state**, not a profile property — all profiles idle at 0/0 rpm at 44–46 °C. [V]

### 5.2 DC — battery (stress-ng CPU-only, 50–80 % charge)

| Slot | idle W | fast W | peak W | **sustained W** | Tctl | fan1/2 |
|---|---|---|---|---|---|---|
| 6 Balanced-DC | 1.3 | 16.5 | 21.8 | **16.9** | 52.3 | 1022/943 |
| 3 Battery Saver | 1.3 | 16.2 | 21.2 | **16.2** | 52.1 | 978/902 |
| 0 (AC val on DC) | 1.3 | 16.1 | 22.3 | **16.3** | 52.0 | 1198/1103 |
| 2 (AC val on DC) | 1.6 | 16.6 | 22.3 | **16.6** | 52.6 | 0/0 |

- **On DC the envelope dominates.** No boost staircase; all profiles clamp to ~16.5 W CPU sustained regardless of value written. The DC envelope sits above every profile. [V]
- Idle package on DC 1.3 W (vs 3.4–4.0 on AC) — powersave stack working. [V]
- The **only** profile axis under isolated CPU load on DC is the **fan floor** (Battery Saver quietest). [V]

### 5.3 The shared package budget (combined CPU+GPU, DC) — the key arbitration finding

Under **combined** load (GPU at ~50 W max on DC), profiles that were identical under CPU-only load diverge:

| DC + GPU@50W | GPU | CPU | interpretation |
|---|---|---|---|
| Balanced-DC (6) | 50 W | ~15 W | GPU priority; CPU takes remainder |
| Battery Saver (3) | 50 W | **~7 W** | GPU priority; **lower total package cap** → CPU halved |

- A **real shared platform budget** exists; both profiles arbitrate **GPU-wins / CPU-yields**; Battery Saver lowers the *total* cap. [O]
- This was invisible in §5.2 because that load was CPU-only. Battery Saver's genuine Linux effect is **total-budget reduction under mixed load**, not a CPU-limit drop — this explains the reported battery-life gains. [O]
- Cross-confirmed by review: GPU ~145 W under combined load "due to power balancing"; CPU "runs at lower wattages when the GPU is fully loaded … necessary in such a slim chassis." [X]

### 5.4 AC — GPU ceilings (KCD2/WoW scan, powerd ON, MangoHud) [O]

| Slot | GPU ceiling observed | note |
|---|---|---|
| 5 / 3 / 6 (+1?) | ~90–95 W | quiet tier |
| 0 Balanced | ~100 W | |
| 2 Performance | ~120–130 W | |
| 7 | ~150 W | **game-limited, not profile-limited** |

- The profile sets distinct **GPU** ceilings too; under gaming load the **GPU axis binds** while CPU sits at 20–30 W (games don't saturate the HX 370, so the CPU tiers of §5.1 stay non-binding). Which ceiling binds depends on workload. [O; CPU tiers V, GPU tiers O]
- **Deliberately not instrumented further:** the GPU ceiling is powerd-dependent, and the operator runs powerd permanently masked (WoW-stall campaign). In the operator's real config the GPU ladder is inactive — the GPU sits at the eco floor regardless of profile. [O]
- Slot 1 ≈ Slot 2 under GPU load too; a GPU-preference for slot 1 (legacy Gaming) is *possible but below scan resolution / unproven*. [H]

### 5.5 Environment / confounders (controlled)

- **No ACPI `platform_profile`** exposed — Razer provides no ACPI power axis. [V]
- **`amd_pmf` loaded** (pulled by `amdxdna` NPU driver) but registers **no CnQF / no slider** — functionless passenger (dmesg shows no PMF/CnQF banner). [V]
- **EC profile is the sole platform-power mechanism.** OS-side only the EPP axis (amd-pstate) exists; kept frozen (PPD balanced / tuned `cachyos-desktop`) for all runs. [V]

## 6. Event choreography (Synapse, host→device)

| Trigger | Packets | Tag |
|---|---|---|
| Profile switch within a domain | SET profile ×2 (zones 01, 02) | V |
| Enter Custom | SET profile(4) ×2, then SET boost CPU, SET boost GPU | V |
| Manual CPU preset change | GET boost CPU, then SET boost CPU | V |
| Manual GPU preset change | SET boost GPU only | V |
| Undervolt ON / OFF | SET boost CPU = 2 (HIGH, locked) / restore stored preset | V |
| **AC plug-in** | SET boost CPU (stored Custom value) + SET profile ×2 (active AC profile) | V |
| **AC unplug** | SET profile ×2 (active DC profile) | V |

The plug-in boost re-assert carries the stored Custom CPU value across sessions (persisted independently of active profile). No GPU boost re-assert on plug-in (asymmetry unexplained). These are Synapse software choreographies, not EC-autonomous behaviour. [V/O]

## 7. Boot default & persistence matrix

- **Profile register is re-initialised to 2 (Performance) on every (warm) boot** [n=2]. Deliberate thermal-safe init. Consequence: a daemon's profile-restore is **mandatory**, not redundant. [V]
- Warm-reboot caveat: EC stays powered across a warm reboot, so this cannot separate EC-RAM from true NVRAM. On an internal-battery laptop a clean cold start is impractical. [note]

| Register | Persistent across (warm) reboot? | BIOS-visible? |
|---|---|---|
| Profile (`0x0d/0x02`) | **No** — re-init to 2 | n/a |
| BHO (`0x07/0x12`) | **Yes** | **Yes** |
| Keyboard colour (tool-set) | **Yes** | Yes (survives into BIOS) |

Selective per-register persistence is by design (BHO must work OS-less; profile must be thermally safe on boot). [V]

## 8. Battery Health Optimizer — resolved

- **Setter `0x07/0x12` alone applies immediately in both directions** (Not charging ⇄ Charging observed live), **persists across reboot**, and is **visible/settable in BIOS**. [V, controlled 2×2 across charge states]
- **Commit `0x07/0x0f` (arg 2) is redundant on 2025** for apply *and* persist. [V]
- The 2024 model reportedly needs the commit ("charging never stops" without it). On 2025 it is a no-op for BHO. A field observation ("profiles unresponsive without commit") was traced to **fan spin-down latency + Performance boot-default**, not the commit — reproduced by setting Silent during charging with an uncommitted BHO transaction and still reaching 0/0 rpm. [V]
- **Kept as default** (design decision 9): Synapse sends it after every battery write; mirroring is harmless.

## 9. Fan subsystem

- **Tach `0x0d/0x88`** confirmed on 2025; ×100; a real tachometer (readings creep, not the target). [V]
- **Spin-down is staged and asymmetric:** fast up, slow down; **zone 2 reaches 0 before zone 1**; total ~60–90 s; charge-independent (n=2). [V]
- Fan floors are the sole DC profile differentiator (§5.2). Manual RPM range **2000–5100** — operator-verified in the Windows Synapse UI [V]; the review’s 1900–5300 [X] and the tool DB’s 2200–5000 are both superseded. `laptops.json` patched accordingly.
- Cooldown-to-idle after an 85 W load is ~4–7 min (not 3).

- **Custom-mode fan runaway = EC firmware bug [V]. → SUPERSEDED by §21 (2026-07-08): reclassified as stuck EC runtime state, cleared by cold boot; Synapse cross-check was state-contaminated.** Original text kept for the record: With profile 4 active the EC drives the fans past 5100 RPM on its own. Reproduced **byte-identically in Windows Synapse** (operator test): selecting Custom in Synapse shows the same runaway, so this is not a tool defect. Interpretation: in Custom the EC runs **no internal curve**; `fanflag=0x00` there means “no manual limit”, not “auto curve” — the host is expected to regulate continuously (Synapse does so in the background, masking the bug). Decision: no software workaround; Custom stays untouched and is excluded from the power-key cycle until a firmware fix lands.

## 10. args[0] (profileId) — resolved

- Synapse sends `0x01`; base tool sends `0x00`. Both accepted.
- **On the profile command (`0x0d/0x02`) the byte is INERT:** a `pid0` write updates the same register a `pid1` read returns; cross-test (`set-profile 0 --pid0` then both reads) confirmed synchronous. [V]
- In the Razer protocol family the byte is named **VARSTORE / profileId**. On *other* commands it may carry store/bank semantics (2024: fan-RPM write with `0x00` accepted but not reliably applied; lighting uses VARSTORE; keyboard colour persistence confirms a store mechanism exists). [O/X]
- **Version-byte hypothesis is dead:** slot-adjacency already provides backward compatibility; profileId is inert on the profile path; the keyboard-persistence anecdote confirms plain store-vs-volatile, no interpretation switch.
- **Recommendation / decision 8:** send `0x01` everywhere (Synapse parity; safe under all remaining hypotheses).

## 11. External validation (third-party reviews) [X]

**Model separation is critical:** the 2025 Blade 16 (HX 370 + RTX 5090) is the operator's machine; the 2026 Blade 16 is Intel Panther Lake — different silicon, same Synapse mode structure.

**2025 AMD (operator's machine):**
- Standard GPU TGP 155–160 W (incl. 25 W Dynamic Boost); sustains ~150 W in gaming. **175 W only with the optional cooling-pad accessory** → directly confirms slot 7 = pad/HyperBoost.
- HX 370: 78 W PL2 / 75 W PL1; Synapse CPU limit adjustable **45–95 W**.
- GPU ~145 W under combined CPU+GPU "due to power balancing"; CPU throttled when GPU loaded → confirms the shared-budget / GPU-priority arbitration (§5.3).
- Presets: Performance / Balanced / Silent / Custom — **no Battery Saver** (confirms it is a firmware-era addition).
- Idle 17 W (Performance) → 13 W (Silent); gaming max 216 W system.

**2026 Intel (structure reference only, NOT operator's silicon):**
- Per-mode: Silent CPU 25 W / GPU 80 W; Balanced GPU 120 W; Performance GPU 165 W / CPU 40 W.
- **~15 % CPU+GPU deficit** on Quiet vs Performance for much lower noise (Cyberpunk TGP 142/115/95 W Performance/Balanced/Quiet) — matches the operator's efficiency thesis quantitatively.

Reviews validate the *physics* (distinct per-mode ceilings, shared budget, pad=175 W, ~15 % efficiency knee) but cover only Windows+powerd; they do not touch the Linux/powerd-off regime, Battery Saver, or the EC protocol — which remain unique to this work.

## 12. Delta: tool vs measured (base `encomjp` and fork `wsquarepa`)

| Aspect | Synapse (measured) | base encomjp | fork wsquarepa |
|---|---|---|---|
| CRC | XOR[2:88] correct | **double bug**: wrong range + written after serialise → sends `0x00` | **fixed** (buf[3:89], stored at buf[89]) |
| transaction_id | rolling 0–30 | constant `0x1F` (= Synapse's reserved boundary) | **rolling 0–30** |
| profileId args[0] | `0x01` | `0x00` | **`0x01`** |
| profile map | §4 (0/2/3/4/5/6, domain-partitioned) | legacy 0–4 labels | legacy 0–4 labels |
| value range | sends 0–6 | `mode<=3 \|\| ==4` → **5,6 unreachable** | **same gate** |
| domain policy | per-source values | none (`ac`/`bat` picks config slot only) | none |
| reply handling | — | read once after 1 ms → **stale-reply trap** | **BUSY/stale polling, request-matched, 20×5 ms** |
| settle delays | J(200)/J(100) | none | **mirrored** |
| resume/AC/dGPU re-assert | — | — | **added** |
| fan-state slot | active-mode-keyed | constant `0x04` → wrong slot | **active-mode-keyed** |

Base sends `crc=0x00` and the EC accepts it → **EC does not validate CRC** (a useful fact, and evidence the base "worked" despite the bug). Fork is the correct patch base: Synapse-faithful transport, correct CRC, rolling tid, profileId=1, robust reply handling. What the fork still lacks is exactly the 2025 map — this document's contribution.

## 13. Patch specification (against `wsquarepa` tree)

Concrete changes, each a single reviewable commit, mapping design decisions to code:

1. **Profile enum → measured 2025 map** with correct names (§4). Remove legacy Gaming/Creator labels.
2. **Remove the `mode<=3 || ==4` gate.** Validate against the measured valid set; allow 5 and 6 to be sent.
3. **Domain policy** (decisions 2–4): expose `{0,2,4,5}` on AC and `{3,6}` on DC; select the correct value from the daemon's existing UPower AC state at send time. No cross-domain exposure.
4. **Slot 1 not exposed** (decision 6); documented internally as Performance-equivalent.
5. **Slot 7 hard-blocked** (decision 7): never sendable, even by raw path in the tool's supported surface.
6. **Custom boost pass-through** (decision 5): CPU/GPU each 0/1/2, no lockout, no undervolt check — send the values as-is. (Smaller than the base: no rule to add.)
7. **args[0] = 0x01** on class-0x0d/0x07 (decision 8).
8. **Boost value 3** dead-code removal (or leave inert) — cosmetic.
9. **Undervolt coupling** (optional): if the tool exposes an undervolt toggle, pair it with a CPU-boost-HIGH write to match Synapse.
10. **Restore paths untouched** (mandatory due to boot default, §7). **BHO commit retained** (decision 9).
11. Doc string / README honesty: for powerd-off users the perceptible profile effect is the **fan curve**, not sustained performance (§5.4).

Non-goals: no fan-curve exposure (decision 10), no DC values on AC, no HyperBoost, no protocol behaviour Synapse doesn't exhibit.

## 14. Open questions (non-blocking)

- **Custom on DC** — untested; likely envelope-dominated (§5.2). [open]
- **Slot 1 vs 2 GPU-preference** — below scan resolution, powerd-dependent, academic for powerd-off operation. [open, low value]
- **args[0] on the fan-RPM command specifically** — 2024 reports it effective there; untested on 2025. [open]
- **Cold-start persistence** — warm reboot cannot separate EC-RAM from NVRAM on this hardware. [open, impractical]
- **Battery Saver full mixed-load quantification** — §5.3 is [O] from a scan; a controlled DC combined-load run (RAPL + nvidia parallel, now that the harness logs `bat_w`) would upgrade it to [V]. [open, optional]

## 15. Appendix — measured tables

### 15.1 Profile map quick reference
```
0 Balanced-AC    exposed (AC)
1 ghost/Gaming    hidden        ~= Performance under CPU load
2 Performance     exposed (AC)
3 Battery Saver   exposed (DC)  new; needs current EC
4 Custom          exposed (AC)  CPU/GPU 0-2 each, all combos (high/high = Performance)
5 Silent          exposed (AC)
6 Balanced-DC     exposed (DC)  same logical profile as 0
7 HyperBoost/pad  BLOCKED       175W, UNSAFE without cooling pad
8+                NOT_SUPPORTED
```

### 15.2 AC CPU ladder (RAPL sustained)
```
tier      slots     sustained   peak     Tctl    fans
quiet     5/3/6     ~35 W       ~48 W    ~63 C   ~2.2k
mid       0         45.6 W      63 W     69 C    2.6k
perf      2/1       ~75 W       ~92 W    ~86 C   4.3k
[unsafe]  7         79.9 W      97.4 W   87 C    4.6k
```
(STAPM skin-temp dependent: Performance 85 W cold-chassis vs 75 W warm.)

### 15.3 DC (CPU-only) — envelope clamps all to ~16.5 W; fan floor is the only differentiator.
### 15.4 DC (combined CPU+GPU) — Battery Saver halves CPU allocation (~15 W → ~7 W) vs Balanced-DC at equal 50 W GPU. [O]
### 15.5 GPU ceilings (powerd ON, scan) — ~95 / 100 / 120–130 / 150 W for quiet-tier / Balanced / Performance / slot7. [O]

---
*Correction log vs earlier working notes: (1) transaction_id is a rolling 0–30 counter, not fixed. (2) The pre-CPU-change "flag packet" is the GET variant `0x87` (read-before-write), not a mode unlock. (3) `balanced-ac`/`balanced-dc` renamed to "Balanced (AC/DC slot)" — same logical profile, two domain values. (4) DC "profiles = fan only" corrected: true for CPU-only load, but Battery Saver lowers the shared package budget under combined load. (5) "Performance idle fan floor" was thermal state, not a profile property. (6) args[0] proven inert on the profile command; version-byte hypothesis retired.*

## 16. Power-mode key (fork feature added in patch v2)

The fn-row power-mode key emits no standard keycode: evdev shows `EV_MSC/MSC_SCAN value 0x700d3` (HID usage page 0x07, usage 0xD3) followed by `EV_KEY KEY_UNKNOWN(240)` press/release [V, evtest capture]. Because KEY_UNKNOWN is ambiguous, the daemon matches the **scancode within the same SYN frame**.

Implementation (daemon `powerkey.rs`): raw evdev via existing `libc` dep; one thread blocking in `poll(2)` over the `/dev/input/event*` nodes whose key-capability bitmap declares **KEY_UNKNOWN (240)** — the input core drops undeclared codes, so the emitting interface provably carries the bit; an earlier `key != 0 ∧ rel == 0` heuristic wrongly excluded composite HID interfaces (Razer keyboards are multi-interface devices) and was replaced [V]; zero idle CPU; 250 ms debounce; press (value 1) triggers.

Cycle semantics (domain-aware, wrap-around, Custom excluded): AC `0→2→5→0` (Balanced→Performance→Silent), DC `6→3→6` (Balanced→Battery Saver). Unknown/Custom active → domain Balanced. The transition is sent through the daemon's **own Unix socket** as `SetPowerMode` — the identical code path CLI/GUI use — so the new profile is **persisted to `daemon.json`** and survives restore (resume/AC-switch/reboot). Stored Custom boosts are read (`GetCPUBoost`/`GetGPUBoost`) and passed through so cycling never clobbers them. OSD feedback via KDE `org.kde.osdService.showText` on the session bus (verified against plasma-workspace `shell/osd.cpp`); silently absent on non-KDE.

## 17. Conditional monitoring & nvidia-smi state-coupling (patch v2.2)

Operator decision (2026-07-05): nvidia-smi returns, but **coupled to the Smart-Curve fan mode** instead of banished. Baseline (fan mode auto/manual): zero sensor reads, zero nvidia-smi, GUI monitor = battery+fan line only. Smart-Curve state: full GTK metric panel (CPU/iGPU/dGPU) + daemon curve GPU-temp via nvidia-smi. All dGPU access double/triple-gated on runtime-active (sysfs) — a suspended dGPU is never woken; curve degrades Both→CPU, GPU→no-op per tick. Rationale: the v2 hwmon substitute was inert (NVIDIA exposes no hwmon node [X, incl. 2026 sources]); a functioning GPU curve source requires nvidia-smi, and coupling it to the only mode that needs it preserves the idle-power invariant everywhere else. GUI gate polls `GetFanCurve{ac}.enabled` per 2 s tick; the monitor's `toolbar` backdrop (whose top edge was the stray hairline above the battery row) toggles with the same gate.

## 18. v2.3 cleanup round (2026-07-05)

Operator-requested full sanity pass. Daemon steady-state fixes: dGPU sysfs path cached (was: PCI directory scan every 2 s in the resume watcher), duplicate `find_dgpu_path` deleted (was: a second full scan per GetGpuStatus → per 2 s GUI poll; classification equivalence proven first), `envycontrol_available` cached (was: one `which` process spawn per 2 s poll), keyboard animator skips the device-manager lock when no effects are active (~30 Hz idle churn removed). Modernisation: `lazy_static` → `std::sync::LazyLock` and unused `systemstat` dropped (two dependencies removed; no new toolchain constraint — edition 2024 already requires ≥1.85). `crash_with_msg` dead code removed → warning-free build expected. powerkey debounce moved to `Option<Instant>`. KDE plasmoid confirmed never-installed (main install.sh has no hook), QML reverted to upstream, declared out of scope; the power-key OSD is the daemon's D-Bus call, unrelated to the plasmoid. About page: "Tested on: Fedora & Arch Linux" (text edit, not a new row), PayPal section + first-run donation dialog removed.

## 19. v2.4 portability round (2026-07-05)

Periphery audit for any-distro/any-DE operation. Findings: udev rule already distro-neutral (hidraw MODE 0666 + uaccess; 0666 kept deliberately — the script's OpenRC path has no logind ACLs). Tray = StatusNotifierItem: native on KDE/XFCE/Cinnamon/LXQt, GNOME requires the AppIndicator extension (Fedora: gnome-shell-extension-appindicator); spawn failure already handled gracefully. New: power-key feedback falls back to org.freedesktop.Notifications (replaces_id reuse, transient hint) where the KDE OSD service is absent — DE-agnostic feedback. install.sh: `systemctl --user daemon-reload` before enable (fresh-install "Unit not found" fix), input-group check with printed remedy, uninstall reordered (stop→remove) and completed (icon SVG, /usr/share/razercontrol dir, daemon-reload, config-kept notice), `main "$@"`. systemd unit and desktop/icon pair verified portable (units are arch-independent; no lib64 concern).

### §19 addendum — upgrade semantics (install-over-install)

Re-running `./install.sh install` is the supported upgrade path (config preserved, unit idempotent, daemon stop→replace→restart). Hardened: binaries via `install -m755` (unlink-first; cp mutates in-use files — glibc-documented crash source, ETXTBSY is a non-contractual courtesy [X, LWN]), `set -e` in both privileged heredoc blocks (previously a mid-block failure was masked by the last command's exit status → silent version-mixed installs), and a running razer-settings/tray process is stopped before replacement. Cumulative-patch corollary: upgrades start from a fresh clone; never re-apply onto a patched tree.

## 20. v2.5 — 2025-only device policy (2026-07-05)

laptops.json 49 → 3 entries (02C5/02C6/02C7); udev rule PID list trimmed to match. Grounds: the measured 2025 wire map replaces the legacy map older models depend on; dual-map support would mean untestable conditional protocol paths. Support is fully data-driven — no code branches existed for specific models [V, grep]. 14/18 2025: same EC generation, inherited data, assumed-compatible/untested; 16 2025 remains the only measurement-verified device. Root README rewritten with verified lineage credits (Razer-Linux original → encomjp → wsquarepa → this fork), diff-derived wsquarepa changelog, this fork's cumulative changelog and binding design decisions.

## 21. Custom runaway reclassified — stuck EC state, not firmware logic (2026-07-08)

Operator observation: the Custom-mode fan runaway vanished with no EC/firmware update, after a
cold boot. Epistemic chain: the original "firmware bug" verdict rested on the Windows Synapse
cross-check (same bytes, same runaway) — which silently assumed a stateless EC. A stuck fan-
controller runtime state (plausibly induced by early raw probing during reverse engineering)
reproduces on any host, contaminating that cross-check. The cold boot cleared the state; whether
via full EC reset or boot-path re-initialisation is unproven [mechanism open]. The v2.6+ TID
correction cannot explain the historical Synapse reproduction and is therefore not the primary
cause. Withdrawn: the §9 interpretation "no internal EC curve in Custom / fanflag=0 means
host-must-regulate" (derived from the runaway). Standing: power-key cycle excludes Custom on
independent grounds (requires boost values; stray keypress must not land there). Remedy playbook
if it recurs: capture RPM + exact preceding command log, switch profile away, cold boot.

## 22. Per-key custom frames — protocol alive, geometry stale (2026-07-11)

On-device probe (flag `per_key_rgb` temporarily set for 02C6, GUI custom tab): the classic
Chroma matrix write — command class 0x03, id 0x0B, `args = [0xff, row, start=0x00, end=0x0f,
…, 45 RGB bytes]`, i.e. the inherited 6-row × 15-key board model — is **accepted by the 2025
EC and renders correctly on every key the inherited map covers** [V, user-observed]. Keys new
to the 2025 keyboard stay dark in custom frames: they lie outside the inherited row/column
map, and the GUI board layout predates them equally. Standard effects (static, spectrum, …)
light **all** LEDs including the 2025-new keys [V, user-observed] — those run in the EC's own
effect engine, proving the firmware addresses the full LED set and only the host-side frame
geometry is stale. Unknown: the true 2025 matrix and whether Synapse 4 still uses 0x03/0x0B
[U]; measurement route if ever wanted: USBPcap of Synapse per-key lighting, or an on-device
coordinate sweep with the confirmed-write transport as oracle. Consequence (v2.10): the probe
motivated the static-only lighting cut — the per-key machinery was removed entirely; the EC
standard-effect STATIC path (0x03/0x0a) is the sole remaining lighting write.

## 23. Legacy static effect applies one-behind — double-write remedy (2026-07-11)

The legacy matrix-effect write (class 0x03, id 0x0A, args `[effect, r, g, b]`) on the 2025 EC
renders **one command behind**: each write stores its parameters and displays the previously
stored ones [V, operator-reproduced: every first GUI apply shows the prior colour, the second
apply — even with a different colour picked in between — shows the first's; time- and
GUI-restart-independent; config persisted correctly on the first apply throughout]. Probe A
(exact data_size 4 instead of the inherited 80) changed nothing [V]. Probe B (extended matrix
0x0f/0x02, openrazer-derived layout `[VARSTORE, BACKLIGHT_LED, 0x01, 0, 0, 0x01, r, g, b]`)
rendered nothing [V]; whether the class is unsupported, the layout wrong, or 0x0f requires its
own transaction-id convention (0x1f/0x9f in openrazer) distinct from the measured 1..=30 cycle
is unresolved [U]. Native 2025 lighting command: measurement route is a USBPcap capture of one
Synapse static colour change. Remedy in force: the confirmed legacy write is sent twice — the
second flushes the first, its own stored copy is identical, cost is single-digit milliseconds.

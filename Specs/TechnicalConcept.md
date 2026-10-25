# Com-IS-Assist — Technical Concept

## 1. Overview

`Goal.md` sets out the target: realtime plugins that automix an International Sound "Bed" (2–6 channels) against a Dialogue "Com" channel (mono), based on the COM/IS loudness-ratio concept from Jonas Engel's bachelor thesis (Hochschule Darmstadt). This document defines *how*.

The existing Reaper JSFX meter (`Ressouces/Com-IS Plugin/COM_IS_RATIO_Meter_5.1_V7.2.txt`) already demonstrates the concept — BS.1770 K-weighting, windowed momentary/short-term loudness, and a COM/IS ratio calculation — but it is **display-only**: there is no gain/duck/reduce/automix code anywhere in it. It measures; it never adjusts levels. It's also locked into Reaper/EEL2 and a single plugin host. The new engineering work is therefore:

1. Get standards-conformant R128/BS.1770 loudness measurement via an existing library, rather than hand-porting the JSFX's implementation.
2. Add the missing automatic gain-reduction layer (the actual "automix") — this part genuinely doesn't exist anywhere yet.
3. Expose that as a VST3 plugin over a **shared DSP core**, so no wrapper can diverge in behavior from another.

### Decisions this concept is built around

- **Prefer existing, well-tested libraries over hand-rolled ports wherever one fits.** R128/BS.1770 loudness measurement → `ebur128` (pure Rust, EBU-conformance-tested). Sample-rate conversion → `rubato` (pure Rust). Denoising → `nnnoiseless` (pure-Rust RNNoise port) / DeepFilterNet's native `libDF`. VAD → Silero via `ort`. The COM/IS ratio-hold policy and the multi-stage automix gain computer stay bespoke (nothing off-the-shelf implements this specific behavior), but their generic building blocks (envelope followers, one-pole smoothers) can still lean on a small permissively-licensed DSP crate instead of being hand-rolled from scratch.
- **Shared Rust core, thin wrappers.** Every wrapper is a thin shell around one `com-is-assist-core` crate — no independent DSP reimplementations. (Two wrappers existed when this was written; it is what kept them identical, and it is why `com-is-assist-offline` can measure the same DSP the plugin runs.) (This project was originally scoped in C++/CMake; it was migrated to Rust after an ecosystem evaluation found an unusually strong fit — see git history for the rationale. The DSP/product design in this document is unchanged by that migration; only the implementation substrate is.)
- **VAD is measurement-only.** The detectors read an internal analysis copy of the dialogue; they decide *when* the automix ducks, never what the audio sounds like. The real Dialogue and Mix outputs stay pristine.
- **The build works on macOS, Windows and Linux** - the three the release workflow produces artifacts for. macOS is where development and Reaper validation happen. (Linux was specifically the deployment platform while the GStreamer element existed - see §7.)
- **The project is open source.** Test fixtures committed to the repo must be synthetic or freely-licensed — real broadcast archive material can be used for local/internal validation but not published. **Licensing is more nuanced than originally assumed**: moving to Rust does not automatically resolve the VST3 target's GPLv3 obligation (`nih-plug`'s default VST3 export still pulls in GPLv3-licensed bindings, for the same underlying Steinberg-SDK-terms reason as JUCE) — see §8 and `docs/LICENSING.md`.

## 2. What we take from the JSFX, and what we now delegate to a library

The JSFX's actual value isn't its filter/windowing code (that's just BS.1770 done by hand in EEL2) — it's the **behavior on top**: which target/tolerance knobs matter, and how the ratio should behave when there isn't enough signal to trust yet. That's what gets carried forward; the low-level DSP is delegated instead of ported:

- **R128/BS.1770 loudness measurement (K-weighting, channel weighting, momentary/short-term windowing, relative gating) → the `ebur128` crate.** A pure-Rust port of libebur128 (MIT-licensed, maintained by a core GStreamer-rs maintainer), already validated against the official EBU Tech 3341 conformance test vectors — a correctness guarantee the original JSFX never had. It handles arbitrary channel layouts via `EbuR128::set_channel_map()` (`Channel::{Left,Right,Center,LeftSurround,RightSurround,Unused}`), and is streaming/incremental by design (`add_frames_f32()` accepts arbitrary chunk sizes), so the "must work with GStreamer/VST3's variable buffer sizes" requirement is satisfied for free.
- **Two deliberate behavioral deviations from the JSFX, both toward standards conformance rather than away from it — worth stating explicitly rather than silently landing:**
  - Surround-channel weighting becomes the standard BS.1770 **+1.5 dB (≈1.1885×)**, not the JSFX's ad hoc **1.2×** approximation. The difference is small (~0.06 dB) but real.
  - The JSFX's `Clspl *= sqrt(2)` compensation on the mono Dialogue channel was a workaround for how its shared summing window handled a single channel; it's an artifact of that specific implementation, not part of BS.1770. Feeding the mono Dialogue signal into its own single-channel `ebur128_state` (channel mapped as `CENTER`) measures it correctly without needing this hack.
- **What stays custom, because nothing off-the-shelf does it**: the COM/IS ratio itself (`ratio = comShortTermDb − bedShortTermDb`) and the policy built on top of it. The JSFX's "hold the ratio steady when there isn't enough valid COM signal yet" gate (`rat_cnt >= 20`) was carried forward as a valid-signal timer and has since been retired — real voice-activity detection (§6) answers "is there COM signal worth reacting to" directly, and does it better than a signal-level proxy ever could. The target/tolerance banding was likewise superseded, by the multi-loop cascade (§5.2).
- **Not carried forward** (confirmed dead or out of scope): `lufs_d_hist`/`lufs_d_gate`/`lufs_d_db` (write-only dead code in the JSFX, never read elsewhere in the script), the stereo Pearson correlation meter, and all `@gfx` drawing code — parameters are exposed as VST3 params, not a from-scratch GUI reimplementation (§8 details one narrow, later exception: a minimal VST3-only GUI for live meter display).

## 3. Repository layout

```
Cargo.toml                  # [workspace], resolver = "2"
crates/
  com-is-assist-core/                # the ONE shared DSP library (lib crate)
    Cargo.toml               # deps: ebur128, ort, rubato
    src/
      lib.rs
      loudness/mod.rs, loudness/ebur128_meter.rs  # thin wrapper around the `ebur128` crate:
                                                    # push_frames(), momentary/short_term/integrated_loudness_db()
      (deleted) ratio/       # was: ratio = com - bed plus a valid-signal hold timer. Removed by the
                              # multi-loop redesign - the ratio is now read straight off the dry meters at
                              # whichever window each stage uses, and the VADs replaced the valid latch.
      automix/               # mix_state (the 4-state machine, §5.1), loop_bank (the 3-stage
                              # cascade, §5.2), k_weighted_loudness (every loudness window the
                              # control path reads - see §9a),
                              # envelope_detector (shared smoother),
                              # automix_engine, gain_ramp, dialogue_mix, processor (shared facade)
      voice_activity/        # SileroVad via `ort` + `rubato` - one instance per side (COM and
                              # IS) - and BackgroundVad, which runs them off the audio thread (§9a)
    tests/                    # automix_engine.rs, com_voice_gate.rs, duck_depth.rs,
                              # ebur128_meter.rs, envelope_detector.rs
  com-is-assist-vst3/                 # nih-plug-based wrapper around com-is-assist-core
  com-is-assist-offline/              # CLI: WAV(bed)+WAV(dialogue) -> CSV metrics + rendered WAVs
    Cargo.toml                # deps: com-is-assist-core (path), hound
    src/main.rs
listening_tests/              # real broadcast material for auditioning changes by ear
docs/                        # ALGORITHM_NOTES.md, DEV_SETUP_MACOS.md, BUILD_LINUX.md, ARCHITECTURE.md, LICENSING.md
```

Everything builds from one Cargo workspace — no ABI boundary is needed between the core and the wrappers (`com-is-assist-vst3`/`com-is-assist-offline` depend on `com-is-assist-core` as a plain path dependency). Note how much smaller `loudness/` got: one wrapper module instead of six hand-rolled DSP classes — and no `third_party/` vendoring tree at all, since Cargo's registry dependencies replace the CMake-era git submodules/vendored libraries entirely.

## 4. Core DSP design

- **`Ebur128Meter`** — a thin wrapper owning one `ebur128::EbuR128` per signal (one for Bed, one for Dialogue), configured with the right channel count/map on stream start via `set_channel_map()` (channel positions read from `GstAudioChannelPosition`/nih-plug's channel-layout metadata, not a fixed pin-index assumption like the JSFX). Audio is pushed in via `push_frames()` (wrapping `add_frames_f32()`) each processing block; `momentary_loudness_db()`/`short_term_loudness_db()`/`integrated_loudness_db()` call straight through to `loudness_momentary()`/`loudness_shortterm()`/`loudness_global()`, sanitizing the non-finite (silence/insufficient-data) case to a `-100.0` sentinel — confirmed against the crate's source that this case surfaces as `Ok(-inf)`, never a distinct error variant, so a plain finite-check is the correct and complete sanitization. This single wrapper replaces the six hand-rolled classes (`Biquad`, `KWeightingFilter`, `SlidingWindowIntegrator`, `GatingHistogram`, `BedLoudnessMeter`, `DialogueLoudnessMeter`) that a from-scratch port would have needed.
- **The COM/IS ratio** is no longer a component of its own. It is simply `COM − BED` in LUFS, read from the dry meters at whichever window a given loop stage uses, and turned into a required reduction inside `AutomixEngine` (§5.2). An earlier `RatioEngine` additionally held a "valid-signal" latch — a debounce deciding when the ratio could be trusted — which has been retired: the COM voice-activity detector (§6) answers that question directly and better, and with feed-forward there is no stale held value to guard against. A reading at the silence sentinel is simply skipped rather than acted on.
- **`KWeightedLoudnessMeter`** (`automix/k_weighted_loudness.rs`) — a K-weighted, ~50 ms loudness detector for the cascade's fast stage. R128 defines only 400 ms and 3 s windows and `ebur128`'s are fixed, so this is bespoke; it deliberately reproduces that crate's K-weighting and channel weighting (including the `1.41` surround factor applied to the *mean square*) so its readings sit on the identical LUFS scale. A unit test asserts the two agree within 0.5 dB on a steady tone, because the fast stage's whole arithmetic depends on that.
- True peak, sample peak, and integrated (relative-gated) loudness are also available for free from the `ebur128` crate if useful as diagnostic/monitoring properties later — not required for the automix control path itself.

**Feed-forward signal flow (this REVERSES the earlier closed-loop design — read this before "fixing" it back):**

Every detector observes the **dry** signal. Bed gain is applied *after* measurement, and nothing downstream of the gain stage is fed back into the control path.

Earlier versions of this document specified the opposite, and called it load-bearing: the Bed meter observed the *already-gained* Bed, on the reasoning that this mirrors a post-fader console meter showing what is actually on air. That reasoning is sound for **metering** and wrong for **control**, and the distinction cost three redesigns to learn. Under feedback the loop cannot observe its own effect until the short-term window has refilled, so every control law had to be slowed to survive a ~3 s measurement lag; speeding it up reintroduced integrator windup and audible pumping, and slowing it down made the mixer feel sluggish and visibly stepped. The architecture, not the tuning, was the problem.

Feed-forward removes it outright. Because reducing the Bed by 1 dB raises the COM/IS ratio by exactly 1 LU, the required reduction is directly computable from the dry levels:

```
required_reduction = target_ratio − (COM_dry − BED_dry)
```

There is no iteration, no convergence behaviour and no stability constraint, because there is no loop. What used to be "one loop that must be slow enough" becomes "several detectors, each free to be as fast as its own window allows" — which is what makes §5.2's cascade possible at all.

The metering argument is preserved separately: `AutomixProcessor::bed_effective_momentary_lufs` reports the dry reading minus the applied reduction, so the GUI still shows what is on air without that value steering anything.

## 5. Automix gain-control design

- **Policy**: automix only *reduces* Bed level (never boosts above unity) — safe ducking semantics.

### 5.1 The state machine

The mixer's behavior is defined by an **explicit four-state machine** (`automix/mix_state.rs`), chosen from the two voice-activity detectors (§6) and nothing else. This replaced an earlier design in which the same decisions were spread across interleaved, order-dependent branches inside `process_tick` (voice-onset fast-trigger → `!voice_active` release → `valid` gate → tolerance dead-band → hold). That older shape was behaviorally defensible but **unpredictable to reason about and to tune**: it was never obvious which branch would win in a given situation. Naming the four situations and handling each in exactly one place is the whole point.

| COM voice | IS voice | State | Behavior |
|---|---|---|---|
| no | no | `ReleaseToUnity` | release toward unity at the normal Recovery time |
| yes | no | `DuckToTarget` | duck Bed to meet `target_ratio_lu` |
| no | yes | `InterviewPassthrough` | release toward unity at the *faster* Interview recovery time |
| yes | yes | `DuckToOvervoice` | duck Bed to meet the higher `overvoice_ratio_lu` |

- **`DuckToOvervoice`** is the double-talk / "over-voice" case: both the commentator and the Bed carry speech, so COM needs more headroom above the Bed than usual to stay intelligible. `overvoice_ratio_lu` is an **absolute** target (default 13 LU), not an offset on `target_ratio_lu` (default 5 LU) — the two situations are dialed in independently by ear.
- **`InterviewPassthrough`** is `UI.md`'s "voice detector IS": voice on the original (IS) while COM is silent means something on the Bed itself is worth hearing — an interview picked up ambiently, say — so it should not stay ducked from prior programme. It recovers at a fixed multiple of the normal rate (`INTERVIEW_RELEASE_SPEEDUP`) and skips the release hold, since getting back to unity promptly is the whole point. **Always active; there is no toggle** — it is part of the state machine rather than a feature to switch on.
- The two release states drive every stage's target to zero (`LoopBank::update_release`); the two ducking states compute each stage's required reduction from the dry levels (§5.2) and drive it there. Nothing else distinguishes them, which is the point of naming them.
- **There is deliberately no voice-onset fast trigger, and no seeding of the reduction on re-entry into a ducking state.** Both were necessary under the old closed loop: its measurement was stale after a gap, so it had to be *told* where to resume from. Feed-forward has nothing stale to compensate for — the dry levels are correct on the very first tick back — and the cascade covers the onset by itself, the mid stage engaging within ~150ms and the fast stage within milliseconds while the slow stage takes over the steady state behind them. Measured on real commentary, deleting the trigger cut the largest single-tick gain movement from 19.4dB to 8.0dB with no loss of onset response. This is one of the clearest wins from the topology change and should not be reintroduced.

### 5.1a Distributing the reduction across the Bed's channels

The cascade produces **one** reduction figure for the whole Bed, from the whole Bed's loudness. `AutomixEngineConfig::duck_depth` then decides how much of it each *group* of channels receives, as a 0-1 factor: 1.0 takes the reduction in full, 0.0 leaves those channels at unity. The groups are **Front L/R, Center, LFE and Surround L/R**, plus **Top Front L/R / Top Rear L/R** on 5.1.4 and **Side L/R / Top Side L/R** on 7.1.2. They are resolved against the `BedLayout`'s channel map, so the same group always means the same speakers. Left and Right must duck by the same amount or the difference is heard as the stereo image sliding sideways, and the same applies to every other pair. The 2.0 plugin exposes no duck depth at all, since a single pair has nothing to distribute across.

Two things make this a distribution control rather than a second gain stage, and both are load-bearing:

- **The measurement is untouched.** Detectors still read the dry Bed as a whole, so a channel held out of the ducking does not change what the loop decides to do - it only changes what happens to that channel. Letting per-channel depth feed back into the measurement would put the windup the feed-forward redesign removed (§5.2) straight back in.
- **Scaling is linear in dB.** `reduction_ch = reduction * depth`, applied as `10^(-reduction_ch/20)`, so 0.5 means half the *decibels*. Scaling the linear gain instead would make the relationship between channels drift with the size of the reduction, and the control would stop meaning anything fixed.

All factors at 1.0 reproduces the pre-feature behaviour exactly, which is the default. The motivation is that the Bed is not homogeneous: surround channels usually carry atmosphere that was never competing with the commentator, and the LFE carries no intelligibility to protect at all, so ducking them as hard as the front sacrifices the scene for nothing.

`DuckDepth::per_channel` resolves the groups to a factor per channel position, against the same map the loudness meters use (`Ebur128Meter::bed_channel_map`), once per config change rather than per sample. That resolution belongs in the core precisely because the mapping is not fixed: position 2 is Center on a 3/5/6-channel Bed but Left Surround on a 4-channel one, so a wrapper exposing positional settings would mislabel them on every layout but the widest.

Applied in `AutomixProcessor::process_bed` at the existing single gain choke point, which now ramps a gain *per channel* (`apply_ramped_gain_per_channel`) and tracks `current_gain` per channel so the channels can sit at different gains without seams between sub-chunks.

### 5.2 Control law: a multi-loop cascade

Three gain stages in cascade, their reductions adding in dB (equivalently, their gains multiplying) — the multi-loop topology used by broadcast dynamics processors, in which several control circuits of differing speed act on the same signal in series. Its defining property is that the effective attack and release times are not configured but **emerge** from how the stages interact, adapting to how the signal evolves. That is why the five separate time parameters earlier designs needed no longer exist.

```
Bed(dry) ─┬─────────────────────► ×g_slow ──► ×g_mid ──► ×g_fast ──► Bed out
          │                          ▲           ▲          ▲
COM(dry) ─┼─────────────────────────────────────────────────┼─────► COM out
          │                          │           │          │
          └──► detectors ────────────┴───────────┴──────────┘
```

Multi-loop designs commonly also place a short delay ahead of the gain stages, so the control circuits see the signal before it reaches the output. **That was implemented here, evaluated on real material and then removed**: the cascade already responds fast enough without it (onset response measured at 0.0–0.1 s), and a lookahead costs exactly its own length in latency. Keeping the signal path strictly unbuffered preserves the element's zero-added-latency property, which the OB-van deployment depends on (§7), and removes a control that earned nothing.

| Stage | Detector | Authority | Ballistics (Speed = 1.0) | Role |
|---|---|---|---|---|
| **slow** | Bed R128 short-term, 3 s | `0 … max_gain_reduction_db` | atk 2.5 s / rel 3 s | programme balance; carries the bulk |
| **mid** | Bed R128 momentary, 400 ms | `±6 dB` trim on slow | atk 0.15 s / rel 0.6 s | tracks Bed dynamics |
| **fast** | Bed K-weighted ~50 ms, **per audio sub-chunk** | `0 … +4 dB` trim on slow+mid | atk 5 ms / rel 0.4 s | catches Bed surges |

Stages are **hierarchical trims**, not three independent controllers: slow states the answer, mid corrects it, fast corrects that. Without the trim structure all three would act on the same error and triple-count it. Bounding each faster stage is also what makes a 5 ms attack safe to have at all — the fast stage cannot run away, because it can never contribute more than its authority.

**Only the Bed is read fast, and this is the single most important detail.** `ratio = COM − BED`, so `required = target − ratio`: when COM gets *louder* the ratio rises and *less* ducking is needed. The event that demands a fast response is therefore a **Bed** surge — a crowd roar burying the commentator — never a COM one. Driving the fast stages from COM as well was implemented and measured, and failed two ways at once:

- At 50 ms a speech signal's "level" is its syllable envelope, so the stage tracked vowels and modulated the Bed at syllable rate.
- A 3 s window over speech-with-gaps measures a systematically quieter COM than a 400 ms window during speech, so the stages disagreed by a near-constant offset and sat fighting: the mid stage parked at −2 to −5 dB while the fast stage repeatedly saturated its clamp.

Reading COM at one stable speed (short-term) for *every* stage fixes both: the stages now measure the same quantity and differ only in how fast they see the Bed move, so they agree in steady state and separate only during a Bed transient. On real commentary this cut total gain movement by 65 % (median 0.88 → 0.31 dB per tick) and fast-stage movement by 70 %.

**The slow stage's attack is deliberately long.** A quicker one does *not* make the mixer more responsive, because the mid and fast stages already cover the first moments (together up to 10 dB, inside one control tick) — it simply arrives on top of stages that are still saturated and overshoots. Measured on a cold start into ~18.7 dB of settled reduction:

| slow attack | cold-start peak | overshoot | ceiling hits |
|---|---|---|---|
| 0.8 s | 24.0 dB (at the ceiling) | +5.3 dB | 13 |
| **2.5 s** | 20.6 dB | **+1.9 dB** | 5 |
| 4.0 s | 18.9 dB | +0.2 dB | 5 |

Onset response is identical at every value (t63 = 0.1 s). 4.0 s removes the overshoot almost entirely but leaves the mid and fast stages pinned at their clamps for longer, with no headroom left to trim; 2.5 s is the compromise.

Supporting details:
- **The fast stage is positive-only.** It may add reduction quickly but never remove reduction the slower stages decided on, because at that time scale a dip in the measurement is usually a gap between syllables rather than evidence the Bed should come up. Clamping at zero is what turns it from an envelope follower into a transient catcher. The cost is a small bias toward over-delivering: measured mean ratio error is **+1.2 LU** against target — erring toward intelligibility, which is the right direction to err.
- **Soft dead-band is gone**; the cascade's own ballistics provide the smoothing a dead-band used to.
- **No voice-onset fast trigger.** The feedback design needed one, because after a gap its measurement was stale; seeding the envelope produced an instantaneous ~19 dB jump. Feed-forward has nothing stale to compensate for, and the cascade covers onsets by itself — mid engages within ~150 ms, fast within milliseconds, slow takes over behind them. Removing the trigger cut the largest single-tick gain movement from **19.4 dB to 8.0 dB** with no loss of onset response.

**Measured on real broadcast material** (60 s commentary over International Sound, `com-is-assist-offline`, channel 1 as COM and channels 2+5 as Bed):

| | value |
|---|---|
| onset response (t63) | **0.0–0.1 s** |
| steady gain movement | median 0.31 dB/tick, slow stage 0.05 |
| ratio held within ±2 LU of target | 67 % of ducking ticks |
| mean ratio error | +1.2 LU |

`speed` is the one remaining timing control, and it scales **both** attack and release. That is safe because the fast-attack / slow-release asymmetry is *structural* rather than something the control has to impose: the cascade bites within one control tick (the fast stage engages in milliseconds), while recovery is governed by the slow stage's constant alone — `update_release` decays the whole cascade by a single factor derived from `SLOW`, and most of the reduction lives in that stage anyway. Measured across the range:

| `speed` | attack (t63) | release (t63) |
|---|---|---|
| 1.0 | <0.1 s | 3.5 s |
| 2.0 | <0.1 s | 2.0 s |
| 4.0 | <0.1 s | 1.3 s |

Scaling both therefore moves the whole envelope without flattening it. Note release does **not** scale a full 4× (3.5 s → 1.3 s, not 0.9 s), because `RELEASE_HOLD_SECONDS` and the ease-in are fixed in absolute time — they are properties of speech-gap length, not of taste — so they automatically cushion recovery at high `speed`.

Scaling only the attack was tried, after a fast recovery at `speed = 4` was noticed as the Bed lurching back up. It does fix that, but it flattens the shape the cascade was built to have and costs the single honest control, so the coupling was restored.

### 5.3 Timing: why hangover and hold must span a sentence pause

The state machine is only as stable as the booleans driving it. With the original 300 ms VAD hangover and 300 ms hold, measured commentary pauses of ~1.2 s dropped COM to "no voice" in **every inter-sentence break**, flipping to `ReleaseToUnity`, shedding several dB and re-ducking on the next sentence — a swell repeating at sentence rate. Defaults are therefore `VoiceActivityConfig::hangover_seconds = 0.8` and, inside `LoopBank`, a fixed `RELEASE_HOLD_SECONDS = 0.8`: together the Bed does not begin recovering until COM has been quiet for ~1.6 s, so natural pauses pass without movement.

**The resistance is an ease-in, not a freeze**, and getting there took two corrections worth recording so neither is reintroduced:

1. *Freezing the cascade outright* for 0.8 s and then releasing normally is the obvious implementation and sounds wrong. It puts a corner in the gain curve — measured at exactly 0.000 dB/tick for 0.8 s, then a step straight to −0.48 dB/tick. A discontinuity in the *rate* of change reads as "nothing happens, then it moves", which is exactly the hold a listener notices. (Making the freeze *more* complete made it more obvious, not less: a leak in the fast stage had been smearing the corner by accident.) The release time constant is therefore **stretched** by `HOLD_RELEASE_STRETCH` at the instant ducking stops and eased geometrically back to 1× over `RELEASE_HOLD_SECONDS`. Recovery begins immediately but far too slowly to hear, then accelerates; short gaps still recover almost nothing.
2. *A negative mid trim must not be allowed to raise the total.* The mid stage is a **signed** trim and is frequently negative while ducking, so a naive release driving it toward zero moves it *upward* — which **adds** reduction, ducking the Bed harder at the exact moment the commentator stopped (+0.6 dB measured on real commentary).

   Each stage nonetheless **releases at its own time constant**, so the multi-loop character carries through the recovery exactly as it does on the way down: the fast stage lets go first, the mid stage next, and the slow stage carries the tail — which is where most of the reduction lives anyway. (The fast stage is released from `update_fast`, at audio sub-chunk rate, so its short constant is realisable rather than quantised to the ~100 ms control tick.)

   What keeps that safe is that a negative mid trim **hands its recovery to the slow stage as it goes**: whatever the mid stage gains back each tick is subtracted from the slow stage, so its return contributes nothing to the total while it still visibly decays at its own rate. Two simpler alternatives were tried and rejected — scaling all three by one shared factor (monotonic, but flattens every stage onto the slow stage's rate) and folding the negative trim into the slow stage in one lump on the first release tick (also monotonic, but snaps the mid readout to zero the instant the state changes, which reads on the meters as a stage having died).

Interview passthrough skips the ease-in, since reaching unity promptly is that state's entire purpose.

Both are deliberately **fixed rather than exposed**. They are properties of speech, not matters of taste, and having the hold as a user control ("Hold time") was part of what made the old parameter surface unmanageable.

### 5.4 Ballistics are emergent, not configured

There is no longer an Attack-Hold-Release envelope on the output, and no attack/hold/release controls. Each stage has its own fixed time constants (table in §5.2) and the *effective* behaviour of the cascade falls out of their interaction: a Bed transient moves only the fast stage, in milliseconds and by a bounded amount; a sustained level change moves all three and the slow stage ends up carrying it. That is the behaviour a multi-loop structure is chosen for — long, gentle attack while the programme is steady, very short attack on an impulsive transient — obtained structurally rather than by tuning.

`EnvelopeDetector` survives as the one shared smoothing primitive, now taking its time constants and time step per call so the three stages can share it while running at genuinely different rates (slow/mid once per ~100 ms control tick, fast once per audio sub-chunk). `GainComputer` is gone.

The single exposed control is **`speed`**, which scales every stage's constants together.

### 5.5 Live reconfiguration

`AutomixEngine::set_config`/`LoopBank::set_config` replace a config in place without resetting any stage's in-flight value or the hold timer — a wrapper can call this every block, cheaply, so host automation (or a dragged slider) actually takes effect. Added after a real bug: VST3's `initialize()` only ever read its parameters once, at plugin load, so raising `max-gain-reduction-db` mid-session silently had no effect. All wrappers now call `AutomixProcessor::set_config` — VST3 every `process()` call.

**No existing library implements this specific ratio-driven ducking policy**, so `AutomixEngine`/`LoopBank` are bespoke — the underlying `EnvelopeDetector` primitive is generic enough that a small permissively-licensed DSP crate could have supplied it, but it's a few lines and hand-rolling it reads better here. The fast stage's K-weighted detector (`automix/k_weighted_loudness.rs`) is likewise bespoke, because R128 defines only 400 ms and 3 s windows and `ebur128`'s are fixed; it deliberately reproduces that crate's K-weighting and channel weighting so its readings sit on the identical LUFS scale.

## 6. Voice-activity detection

**Implemented: real voice-activity detection on *both* the Dialogue (COM) and Bed (IS) sides.** `com_is_assist_core::voice_activity::SileroVad` wraps Silero VAD (a small recurrent ONNX model, vendored in-repo at `crates/com-is-assist-core/assets/silero_vad.onnx` - MIT-licensed, see `docs/LICENSING.md`) run through `ort` (the one place a C++ library - ONNX Runtime itself - sits underneath). Audio is resampled to Silero's required 16kHz mono via `rubato` (pure Rust), fed through in the model's native 512-sample (32ms) chunks with the 64-sample inter-chunk context and recurrent state it expects, and the resulting per-chunk speech probability is smoothed by a hangover timer (`VoiceActivityConfig::hangover_seconds`, **default 800ms**) into a stable `voice_active` boolean. That hangover is effectively "how long a pause still counts as *still talking*", so it has to span a natural **inter-sentence** break, not just an inter-word one — it directly gates the §5 state machine, and at the original 300ms every ~1.2s commentary pause dropped to `ReleaseToUnity` and caused an audible sentence-rate swell (see §5.3).

**Guard band on the COM side.** A detector answers "is this speech?", not "is this speech *on this microphone*". A commentary mic picks up the programme bed as crosstalk, and that spill genuinely is speech — so the VAD quite correctly fires on it, and the Bed then ducks itself against its own bleed. `AutomixEngineConfig::com_gate_lufs` suppresses reported COM voice while COM's own **momentary** loudness sits below it (momentary rather than short-term, so the gate tracks the talker rather than lagging seconds behind). It ships **engaged** at -32 LUFS rather than at the bottom of its range: a gate parked at -70 protects nothing, and this is the failure it exists to prevent. Level is the right discriminator here precisely because spill is reliably far quieter than the commentator, while being indistinguishable from speech by content — which is also why this sits *alongside* the VAD rather than replacing the LUFS-floor proxy the VAD was brought in to kill.

Applied in `AutomixProcessor` (that is where COM's measured level lives), so all three wrappers inherit it identically, and the **gated** flag is what both the state machine and the GUI indicator see — a lit LED while the guard band was suppressing detection would be a lie. Default −70 LUFS, effectively open: real programme audio sits far above it, so behaviour is unchanged until it is deliberately raised. Useful settings are typically 20–30 dB below the commentator's own momentary level; measured on real commentary (median −26 LUFS), a −40 LUFS gate removes about 10% of detections, which are the quiet tails and spill.

**Two instances, one per side.** The COM detector gates the ratio/automix loop as before. The **IS detector** (`UI.md`'s "voice detector IS") is fed a **mono downmix of the dry Bed** — Silero is a mono model, and "is anyone speaking anywhere on the Bed" is a whole-Bed question, so averaging the channels is the right reduction. It is deliberately fed the *dry* Bed, before automix gain is applied, so IS detection never depends on how hard the mixer happens to be ducking (which would close an unwanted second feedback path). Together the two booleans select the automix state (§5.1), which is what makes the interview-passthrough and over-voice cases possible at all. Each wrapper owns both instances and passes both into `AutomixProcessor::maybe_run_control_step(com_voice_active, is_voice_active)`. COM fails **open** (treated as always voiced if the detector couldn't be constructed, matching pre-VAD behavior); IS fails **closed** (treated as never voiced), which simply leaves the ordinary duck/release states in play rather than inventing an interview/over-voice state.

That boolean now feeds `MixState::from_vad` directly (§5.1) - it replaces what was previously derived from "is COM's short-term loudness above -70dB LUFS?" A loud non-speech noise (room tone, static, another open mic) no longer gets treated as "the target dialogue is present," which the LUFS floor fundamentally couldn't distinguish. `AutomixProcessor` doesn't own the VAD itself - each wrapper (VST3 and `com-is-assist-offline`) owns its own `SileroVad` instance (fed dialogue audio directly, independent of Bed's own state/caps-negotiation lifecycle) and passes the resulting `voice_active` into `AutomixProcessor::maybe_run_control_step` explicitly, the same way `automix_enabled` already was - keeping `AutomixProcessor`'s own tests decoupled from needing real speech audio to exercise the ratio/gain logic, while every wrapper still shares the exact same detection implementation (no risk of the *algorithm* diverging, even though each holds its own instance). Fails open (treats COM as always voice-active, matching the pre-VAD behavior) if the ONNX Runtime binary couldn't be obtained at startup, rather than silently never ducking at all.

**No denoising.** An `nnnoiseless`/DeepFilterNet path feeding a separate `Cleaned` measurement was part of the original concept and was never built. The VAD alone turned out to answer the question the automix actually asks - "is the commentator talking?" - without it.

## 7. GStreamer element (removed)

The project originally shipped a GStreamer element (`comisassist`, 2 sinks / 3 srcs via `GstCollectPads`) as the primary broadcast deployment target, with the VST3 plugin as the DAW-side validation tool. **It has been removed** - the deployment need it existed for went away, and carrying a second wrapper means carrying a second way for behaviour to diverge. The crate, its spec (`GSTdefinitions.md`), its CI workflow and its `cargo-c` build path all went with it. Git history has the full implementation and the design notes that went with it if it is ever wanted back.

Two things it taught are worth keeping, because they constrain the core rather than that wrapper:

- **Zero added latency is a property of the core, not of a wrapper.** Output latency was deliberately decoupled from the automix control rate: audio is gained and forwarded as it arrives, with the control target recomputed once per `TICK_SECONDS` independently of how output happens to be chunked. The first version batched a full tick before emitting anything, which a live pipeline with a real-time sink audibly glitched on. `AutomixProcessor` still works that way and still adds nothing.
- **The control rate floor is not arbitrary.** BS.1770/R128 gates loudness in 100 ms blocks internally, so ticking faster buys no new loudness information - only overhead.

## 8. VST3 wrapper design

**`nih-plug`** over JUCE: a real, actively maintained Rust plugin framework targeting VST3 and CLAP directly, with a bundler (`cargo xtask bundle`), GUI adapters (egui/iced/vizia) if ever needed, and sample-accurate automation support. `nih-plug` is less battle-tested than JUCE for VST3 specifically, but that's an acceptable risk. The core-wrapping structure enforces "no DSP/parameter divergence": both wrappers call into the same `com_is_assist_core::automix::AutomixProcessor` (loudness meters, ratio/gain engines, continuous gain ramp — extracted out of what was originally duplicated wrapper logic, once a second wrapper needed the identical control loop) rather than each re-deriving its own copy.

**Bus layout — Bed on the main bus, COM on a mono sidechain, one plugin per bed format.** The plugin originally used one fixed 8-in/8-out bus (Bed 0-5, Dialogue 6, unused 7). It now takes the Bed as the main input and output and the COM as a mono VST3 aux (sidechain) input. One binary exports four plugins: 2.0 (2ch), 5.1 (6ch), 5.1.4 (10ch, `k51_4`) and 7.1.2 (10ch, `k71_2`). Each has exactly one fixed layout, instead of one plugin with several host-negotiated layouts. Each plugin reports and accepts exactly one VST3 speaker arrangement (`kStereo` / `k51` / `k51_4` / `k71_2`, sidechain `kMono`), which requires the patched nih-plug in `vendor/nih_plug`. See `Specs/vstDefinitions.md` "Bus structure" for the reasoning. In the core, the layout is a `BedLayout` rather than a channel count, because 5.1.4 and 7.1.2 are both ten channels; `bed_channel_map(count)` still exists and reads 10 as 5.1.4. The channel positions follow ITU-R BS.2051: 5.1.4 heights at U±030 / U±110, 7.1.2 sides at M±090, its Ls/Rs at M±135 and its top pair at U±090. BS.1770 weights elevated channels at 1.0 and gives the +1.5 dB surround weight to the 7.1.2 side pair.

A `mix-dialogue-to-bed` bool parameter (the "bus insert mode" toggle) controls whether the main output bus is leveled Bed alone (for inserting directly on a Bed/IS channel strip) or have Dialogue mixed in (matching `mix_src` — for standalone use as the final program bus). When mixed in, Dialogue is restricted to Left/Right/Center only — it never reaches the LFE, surrounds, sides or heights — placed via a `divergence` parameter (`UI.md`'s "Voice divergence": 0% = Center-only, 100% = split Left/Right) using `com_is_assist_core::automix::mix_dialogue_into_bed`'s equal-power pan law, extracted as a shared function for the same "no DSP divergence" reason as `AutomixProcessor`. On 2.0 there is no Center, so Dialogue is split equally between L and R and divergence does not apply. Dialogue is not passed through on its own: it arrives on the input-only sidechain, and already has its own path in the host. A separate `bypass` bool parameter disables automix entirely (Bed passes through at unity gain) when `true` — named for the conventional meaning of a "Bypass" control.

**Control surface.** The multi-loop redesign removed five timing parameters (`adaptation-ms`, `attack-ms`, `hold-ms`, `release-ms`, `interview-release-ms`) because the cascade's ballistics are emergent (§5.4), and replaced them with one **`speed`** macro. The full VST3 surface is now `target-ratio`, `overvoice-ratio`, `max-gain-reduction-db`, `com-gate-lufs`, `speed`, `bypass`, `mix-dialogue-to-bed`, `divergence` — eight, down from twelve, with the five that were hardest to reason about gone.

**Resolved: a custom GUI, covering both live meters and the parameter controls.** Live meters can't be exposed as parameters — `nih-plug`'s parameter setter (`ParamMut`) is deliberately `pub(crate)`, so a plugin cannot programmatically update its own parameter's displayed value from `process()` — that's why a custom GUI exists at all (the original concept had none - parameters only). Implemented via `nih_plug_egui`: a `Meters` struct of `Arc<AtomicF32>` fields, written from `process()` only while the editor is open and read each frame in `editor()`. Redesigned (visual/layout only, no DSP change) into a tall, professional-bargraph-style meter bank on the left of the window with all parameter controls on the right — loosely modeled on the original Reaper JSFX reference meter and professional loudness-meter hardware (e.g. RTW), not on peak-meter-ballistics standards. Four vertical bar meters, left to right: gain reduction, IS, ratio, COM — each 420px tall, narrow, and drawn with a fixed-pitch segmented-gap overlay for a discrete-LED look — sharing one visual language (filled rect over a dark background rect), gain reduction/IS/COM each paired with an adjacent tick-mark scale:
- **Gain reduction** — its own standalone bar (not carved out of the IS bar, as an earlier iteration did), fills top-down from 0 to `applied_gain_reduction_db()` against a fixed 0-36dB range, red, with its own top-down scale. Made standalone specifically so gain reduction and ratio can both be read clearly at a glance while tuning attack/hold/release by ear.
- **IS (Bed)** — fills bottom-up, cyan, fixed -60..0 LUFS range, to `AutomixProcessor::bed_momentary_lufs()`: the dry/incoming programme level, which since the move to feed-forward is the *same* meter the control path reads (the old closed-loop design needed two).
- **Voice-activity LEDs and state readout** — a green/grey LED under the COM bar *and* under the IS bar (the latter unblocked by the IS-side VAD now existing, §6), plus a live text readout of the current `MixState` in the controls column. With behavior now defined entirely by which of four states the mixer is in, showing that state directly is the fastest available answer to "why is it doing that right now?" — considerably more legible than inferring it from the meters.
- **Ratio** — positioned between IS and COM, drawn as a bar spanning the gap between Bed's *effective* level and `bed_effective_momentary_lufs() + display_ratio_lu` on the *same* LUFS scale (LU is literally a LUFS difference), colored by a four-state GUI-only read of where `AutomixProcessor::applied_ratio_lu()` — the ratio the control is actually achieving, i.e. the dry ratio plus the applied reduction — sits relative to whichever target is active (`AutomixProcessor::active_target_ratio_lu`, so the over-voice state is coloured against its own higher target): grey when negative (COM quieter than IS), red when under target, green on target, orange above it. The bar's *height*, however, uses a separate `AutomixProcessor::display_ratio_lu()` (dialogue momentary minus Bed's *effective* momentary, floored at 0) rather than `applied_ratio_lu` directly — the two are computed from different meters/time-constants entirely, so using `applied_ratio_lu` (or anchoring against Bed's raw pre-gain level) made the bar inconsistent with what's actually audible — `display_ratio_lu` fixes that by matching what the ear (and the IS/COM bars) actually perceive. Floored at 0 rather than signed: COM having a brief natural gap below Bed's effective level isn't itself alarming and isn't worth showing as a negative "distance."
- **COM (Dialogue) momentary loudness** — same as IS, orange, via `AutomixProcessor::dialogue_momentary_lufs()` (never gained, so no pre/post distinction on this side).

Integrated LUFS was tried in an earlier iteration and dropped as an unneeded simplification. Parameters (target ratio, max gain reduction, attack/hold/release, voice divergence, bypass, mix-dialogue-to-bed) are exposed directly in the same window via `nih_plug_egui`'s `ParamSlider` (and a hand-rolled `ui.checkbox` + `ParamSetter` pairing for the two bool params, since `nih_plug_egui` has no built-in bool widget) — still routed through the host's automation via `ParamSetter`, not a GUI-only shadow state, so host automation/undo continues to work identically. See `Specs/vstDefinitions.md`.

Since `nih-plug`'s `Buffer` is planar (one slice per channel) while `com_is_assist_core` works in interleaved audio, the VST3 wrapper de-interleaves Bed into a scratch buffer before calling into `AutomixProcessor` and re-interleaves on the way out — the only real translation layer needed.

**Building/bundling**: `nih-plug` doesn't publish to crates.io — it's a git dependency (`nih_plug = { git = "https://github.com/robbert-vdh/nih-plug.git" }`). Bundling uses `nih-plug`'s own `cargo xtask bundle` mechanism: a small `xtask` crate (workspace member, wraps `nih_plug_xtask::main()`) plus a `.cargo/config.toml` alias and a root `bundler.toml` naming the plugin. `cargo xtask bundle com-is-assist-vst3 --release` produces a real, correctly-signed `.vst3` bundle (confirmed: valid arm64 Mach-O binary, loadable bundle structure) with no additional tooling beyond what `nih-plug` itself provides.

**The VST3 target carries a GPLv3 obligation.** `nih-plug` itself is ISC-licensed, but its default `vst3` feature pulls `vst3-sys`, which is GPLv3 - it wraps Steinberg's VST3 SDK under that SDK's older dual-license terms. Nothing in this project's own code requires it. `docs/LICENSING.md` is authoritative, including what would remove the dependency rather than work around it.

**The project is licensed per crate, not project-wide** (`docs/LICENSING.md` is authoritative): MIT for `com-is-assist-core`, `com-is-assist-offline` and `xtask`; GPL-3.0-or-later for `com-is-assist-vst3` alone. Verified with `cargo tree` rather than assumed - the non-plugin crates pull no `vst3*` dependency at all, so the GPLv3 reach really is confined to the plugin wrapper and the split needed nothing restructured, only declared. Shipping CLAP instead of VST3 would remove the obligation entirely (`clap-sys` is MIT/Apache-2.0 and already in the tree).

## 9. Build system

Cargo workspace (`Cargo.toml` at the repo root, `resolver = "2"`). Cargo replaces CMake entirely — nothing meaningful is lost:

**The `block v0.1.6` future-incompatibility warning on every VST3 build is upstream and expected.** `block` declares `static _NSConcreteStackBlock: Class` where `Class` is uninhabited, and rustc is phasing out uninhabited statics ([rust#74840](https://github.com/rust-lang/rust/issues/74840)). It arrives via `cocoa` → `baseview` → `egui-baseview` → `nih_plug_egui`, all pinned to specific git revisions inside nih-plug's own dependency chain, and only on macOS.

Nothing in this project triggers it and there is nothing to bump to: `block` 0.1.6 is that crate's last release, the ecosystem having moved to `block2` under `objc2`. Verified scope: `com-is-assist-core` and `com-is-assist-offline` build with **zero** warnings - only the macOS GUI path is affected.

It is a warning today and a hard error once the lint graduates, at which point the macOS VST3 build breaks until nih-plug's chain updates. If that lands before upstream moves, the escapes are a `rust-toolchain.toml` pin (there is deliberately none today) or a `[patch]` onto a forked `block`. Neither is worth doing pre-emptively.

- Dependency fetching: Cargo's registry deps replace CMake's `FetchContent` (previously used only for Catch2) — no fetch-workaround needed at all now that `cargo test` is built in.
- `ebur128`/`hound` are pure-Rust crates.io dependencies — no `pkg-config`/Homebrew package needed for either (a simplification versus the old `libebur128`/`libsndfile` C-library story).
- Build-time toggles (previously `COMIS_BUILD_GSTREAMER`/`COMIS_WITH_DENOISE_RNNOISE`/etc. CMake options) become Cargo features on `com-is-assist-core` plus workspace member selection (`cargo build -p com-is-assist-gstreamer`).
- **ONNX Runtime (for Silero via `ort`) is not a target/runtime dependency** - resolved differently than originally planned here. `ort`'s `download-binaries` feature fetches a prebuilt ONNX Runtime *static* library at build time (cached after the first fetch, so only the build machine ever needs network access for it, once) and statically links it directly into the compiled binary - verified with `otool -L` against the actual built VST3 artifact, which shows no `libonnxruntime` dependency at all. No Homebrew package, no target-side install step, and the deployed artifact runs fully offline. (DeepFilterNet's primary path, `libDF` + `tract`, still needs no such dependency either way, since `tract` is pure Rust - not yet implemented, see section 6.)
- Silero's small `.onnx` vendored in-repo (MIT-licensed, redistributable, `crates/com-is-assist-core/assets/silero_vad.onnx`, embedded via `include_bytes!`); larger DeepFilterNet models would be fetched via a pinned-SHA256 build step rather than committed, if/when that path is built.
- macOS dev loop: `rustup` (toolchain) alone - no `onnxruntime` package needed (see above) and nothing else to install - then standard `cargo build`/`cargo test` and `cargo xtask bundle com-is-assist-vst3 --release`. (A `docs/DEV_SETUP_MACOS.md` was referenced here but never written; the README's Building section covers it.)

**There is no CI test run.** The only workflow is the release build, triggered by a tag - nothing runs on an ordinary push, by deliberate choice. `cargo test --workspace` and `cargo clippy --workspace --all-targets` are therefore a local responsibility before committing.

**Release builds** come from `.github/workflows/release.yml`, triggered by a `v*` tag: macOS (arm64), Windows (x64) and Linux (x64), one zip per platform attached to the GitHub release. The tag is passed as `COMIS_RELEASE_VERSION` and stamped into the plugin by `build.rs` (§8).

**There is no Intel macOS build, and cannot be one without extra work.** `ort`'s prebuilt ONNX Runtime distribution list (`ort-sys`'s `build/download/dist.txt`) has `aarch64-apple-darwin` but no `x86_64-apple-darwin`, so the Silero VAD dependency has nothing to link against there. This also rules out `cargo xtask bundle-universal`, which would otherwise be the obvious way to ship one macOS artifact. Supplying an ONNX Runtime build for that target by hand is the only route.

Linux needs a long list of X11/XCB/GL development packages for `baseview`/`egui`/`winit`. nih-plug's own CI list is the starting point but is **not sufficient here**: the `x11` crate's build script runs `pkg-config gl`, and `libgl-dev` on `ubuntu-22.04` does not provide `gl.pc` - `mesa-common-dev`/`libgl1-mesa-dev` do. The workflow checks the pkg-config packages it needs *before* compiling, so a future gap fails in seconds with the package named rather than minutes later inside a build-script panic. Nothing else is needed on any platform: ONNX Runtime is downloaded by the build and statically linked (§9).

## 9a. Real-time cost

The plugin was reported as "very DSP hungry" in a live host, needing its buffer raised from 64 to 128 samples. Measured (`cargo run --release --example bench_realtime -- 64`), the cause was not average load but **spikes**: mean cost was 2.3% of a 64-sample budget while the worst block was **204%** of it. A real-time host must size for the worst block, so a plugin can look cheap on a CPU meter and still force the buffer up.

Three periodic spikes, all now removed:

| | was (worst block) | now | fix |
|---|---|---|---|
| Control step | 1318us (99% of budget) | 0.5us | incremental loudness windows |
| COM VAD | 941us | 4us | inference moved off the audio thread |
| IS VAD | 731us | 21us | same |
| `process_bed` | 133us | 50us | one K-weighting pass instead of two meters |
| Wrapper buffers | 15us | — | preallocated; no allocation on the audio thread |
| **Total** | **2718us (204%)** | **47us (3.5%)** | |

**The loudness query was the biggest single cost and the least obvious.** `libebur128` recomputes a window's sum of squares on every call rather than maintaining it: measured at ~700us for short-term and ~100us for momentary on a 6-channel bed, and the control step asks three times per tick. `KWeightedLoudnessMeter` now keeps the *same rectangular windows* as running totals, adding the newest millisecond of energy and subtracting the one ageing out - O(1) per query. The window shape is deliberately unchanged, so the cascade's ear-tuning carries over; an exponential average would have been simpler and was rejected for exactly that reason. Agreement with `ebur128` is asserted on steady tones (<0.1 dB), through repeated level steps (<0.5 dB), and measured at **0.043 dB** across real programme material (`cargo run --release --example compare_meters -- <file.wav>`).

**ONNX inference cannot be made cheap, only moved.** A Silero inference costs 600-900us and runs once per 32ms, so at a 64-sample buffer it lands wholly inside one block in twenty-four - and the two detectors can spike on the same one. `voice_activity::BackgroundVad` runs the model on a worker thread, leaving the audio thread a bounded copy into a lock-free SPSC ring. It costs up to 10ms of extra detection latency, immaterial against the detector's own 32ms hop and 800ms hangover, and it counts dropped samples so a worker that cannot keep up is visible rather than silent.

`Ebur128Meter` is no longer in the audio path at all. It remains as the conformance reference the incremental meter is tested against.

## 9b. Detection latency

Reported as the bed ducking audibly late. Measured on real commentary by splicing speech at a known sample after silence (`cargo run --release --example detect_latency -- <mono.wav>`), the delay from speech onset to the bed starting to move was **~150-300ms**. Four causes, three of them ours:

| Cause | Was | Now |
|---|---|---|
| Resampler input chunk | 100ms | 10ms |
| Mix state only adopted on the control tick | up to 100ms | 0 |
| `LoopBank::releasing` only flipped on the control tick | up to 100ms | 0 |
| `BackgroundVad` poll interval | up to 10ms | up to 2ms |
| Silero's own 32ms inference hop + confidence | ~32-180ms | unchanged |

Measured result: **onset-to-duck median 99ms -> 49ms, mean 140ms -> 85ms**, and the control path now adds *nothing* on top of the detector - the gain starts moving on the same block the flag rises.

Two of these were the same mistake in different places: a value that only changes at the ~100ms control tick being consulted by code that runs per audio sub-chunk. The tick is 100ms because that is how often **R128 loudness** carries new information; the VAD and the state machine have something new to say far sooner, and should not have been quantised to it. `dialogue_reference_lufs` was a third instance, cached at tick rate purely because querying `ebur128` was expensive (§9a) - a reason that no longer existed.

**The remaining ~49ms is Silero's**, and cannot be optimised away: the model consumes 512 samples at 16kHz, so no answer exists until 32ms of audio has arrived, plus however long it takes to commit (the slowest onsets measured ~209ms on soft speech). Going below that needs look-ahead, which is **not** implemented and was rejected rather than deferred:

- Delaying only the Bed desynchronises it from picture. The Bed is the match sound; a 40ms offset against video is a lip-sync error.
- Delaying everything puts latency in the commentator's own monitor path, which is the one place a live operator will not accept it.
- Zero added latency is otherwise a property of the whole signal path (§7), relied on in live use.

## 10. Testing and validation

The numbered items below are what the design is actually held to; 3 and 4 run on every change. Two `examples/` in the core are the real-time tooling: `bench_realtime` reports per-block cost against the budget by percentile (the *worst* block is what a host is killed by, not the mean), and `compare_meters` checks the incremental loudness windows against `ebur128` on a real file.

1. **Golden-reference comparison against the JSFX meter** — now a *ballpark sanity check*, not a bit-exact port validation, since §2 documents two deliberate standards-conformance deviations (surround weight, no sqrt(2) mono hack). Add a debug-logging JSFX fork (`tests/golden/reaper/..._LOGGING.jsfx`, never touching the real reference file) dumping per-tick LUFS/ratio values to CSV, and compare against `com-is-assist-offline`'s output with a tolerance wide enough to absorb the known, intentional deltas (document the expected delta magnitude somewhere durable so a widening tolerance isn't mistaken for a bug later). Because the project is open source, all fixtures used here must be synthetic or freely-licensed audio — not real SRF broadcast archive material, which can still be used for local/internal validation but never committed to the public repo.
2. **Conformance testing "for free"**: since the `ebur128` crate is already validated against the official EBU Tech 3341 test vectors, running those same public vectors through `Ebur128Meter` gives a real standards-conformance check that the original JSFX never had — worth adding as its own `cargo test` target independent of the JSFX comparison.
3. **Unit tests (`cargo test`)**: `Ebur128Meter` against known-level synthetic test tones (expected LUFS within tolerance: a 400 Hz full-scale sine measures ≈-3.7 LUFS), `KWeightedLoudnessMeter` agreeing with `Ebur128Meter` on a steady tone (the scale the fast stage depends on), `MixState`'s full truth table, and `AutomixEngine`'s feed-forward exactness, per-stage authority limits, release monotonicity and ease-in.
   - **Duck depth** (`tests/duck_depth.rs`) drives the real `AutomixProcessor` rather than the gain helper, since the property worth protecting is end-to-end: the depth a wrapper sets is the depth the audio receives, in dB, after the ramp and the cascade have had their say. Two of the five are about the grouping rather than the gain - that `DuckDepth::per_channel` follows the *channel map* across all five supported channel counts (position 2 is Center on a 3/5/6-channel Bed but Left Surround on a 4-channel one), and that the front and surround pairs can never diverge, which is the guarantee against hearing the stereo image slide sideways.
   - **Shipped defaults are pinned** (`com-is-assist-vst3`'s own unit tests): one test asserts every default outright, another checks the VST3's against `AutomixEngineConfig::default()` so the two wrappers cannot disagree about out-of-the-box behaviour. These exist because the defaults have moved more than once, and because a value on a plugin's GUI gives no clue whether it is the shipped default or something the host restored from a saved session - so it is a question the plugin cannot answer by inspection.
   - **Two tests specifically guard against the pumping regression** (§5.2), since it is the defect this design exists to fix and it would be easy to reintroduce: one asserts the settled reduction is *exactly* what the dry levels imply (`target − (COM − BED)`), which is only a well-defined question at all because the path is feed-forward; another asserts a silence-sentinel reading holds rather than driving the gain. Further tests assert that `speed` actually governs responsiveness (it is the only timing control left), that release never *adds* reduction, and that release eases in rather than stepping — the last two being real defects found by listening, not hypotheticals.
4. **Empirical pumping measurement**: `com-is-assist-offline` emits per-tick CSV - `tick,time_s,bed_lufs_m,com_lufs_m,com_voice_active,is_voice_active,mix_state,ratio_lu,target_lu,r_slow,r_mid,r_fast,gain_reduction_db`. The three per-stage columns are what make the cascade legible: a problem usually shows up as one stage doing work that belongs to another (the fast stage thrashing, or the slow stage carrying a transient), which the summed `gain_reduction_db` alone hides. Config is overridable from the environment (`COMIS_TARGET_RATIO_LU`, `COMIS_OVERVOICE_RATIO_LU`, `COMIS_MAX_GAIN_REDUCTION_DB`, `COMIS_COM_GATE_LUFS`, `COMIS_SPEED`, `COMIS_DUCK_DEPTH` - up to eight comma-separated factors in the order `front,center,lfe,surround,side,top_front,top_rear,top_side` (`side`/`top_side` act on 7.1.2 only, `top_front`/`top_rear` on 5.1.4 only), blank fields keeping their default - `COMIS_BED_LAYOUT=7.1.2` to read a 10-channel Bed as 7.1.2 rather than 5.1.4, and `COMIS_VAD_HANGOVER_SECONDS`) so a sweep needs no rebuild. Running it over a speech-with-gaps-over-loud-Bed fixture and counting direction reversals / total gain churn is how the §5.2 table was produced; re-run it after any change to the control law or the timing defaults.

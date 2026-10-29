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
                              # feed-forward redesign - the ratio is now read straight off the dry
                              # short-term meters, and the VADs replaced the valid latch.
      automix/               # mix_state (the 4-state machine, §5.1), gain_loop (the one
                              # feed-forward loop, §5.2), k_weighted_loudness (every loudness window the
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
- **The COM/IS ratio** is no longer a component of its own. It is simply `COM − BED` in LUFS, read from the dry short-term meters, and turned into a required reduction inside `AutomixEngine` (§5.2). An earlier `RatioEngine` additionally held a "valid-signal" latch — a debounce deciding when the ratio could be trusted — which has been retired: the COM voice-activity detector (§6) answers that question directly and better, and with feed-forward there is no stale held value to guard against. A reading at the silence sentinel is simply skipped rather than acted on.
- **`KWeightedLoudnessMeter`** (`automix/k_weighted_loudness.rs`) — R128's momentary (400 ms) and short-term (3 s) windows on the identical LUFS scale as `ebur128` (its K-weighting and channel weighting, including the `1.41` surround factor applied to the *mean square*), but maintained incrementally so a query is O(1) (§9a). It is what the gain loop and the meters read; `Ebur128Meter` remains the conformance reference it is tested against. It once also carried a ~50 ms detector for a millisecond-scale gain stage, removed with that stage (§5.2).
- True peak, sample peak, and integrated (relative-gated) loudness are also available for free from the `ebur128` crate if useful as diagnostic/monitoring properties later — not required for the automix control path itself.

**Feed-forward signal flow (this REVERSES the earlier closed-loop design — read this before "fixing" it back):**

Every detector observes the **dry** signal. Bed gain is applied *after* measurement, and nothing downstream of the gain stage is fed back into the control path.

Earlier versions of this document specified the opposite, and called it load-bearing: the Bed meter observed the *already-gained* Bed, on the reasoning that this mirrors a post-fader console meter showing what is actually on air. That reasoning is sound for **metering** and wrong for **control**, and the distinction cost three redesigns to learn. Under feedback the loop cannot observe its own effect until the short-term window has refilled, so every control law had to be slowed to survive a ~3 s measurement lag; speeding it up reintroduced integrator windup and audible pumping, and slowing it down made the mixer feel sluggish and visibly stepped. The architecture, not the tuning, was the problem.

Feed-forward removes it outright. Because reducing the Bed by 1 dB raises the COM/IS ratio by exactly 1 LU, the required reduction is directly computable from the dry levels:

```
required_reduction = target_ratio − (COM_dry − BED_dry)
```

There is no iteration, no convergence behaviour and no stability constraint, because there is no loop. What used to be "a loop that must be slow enough to stay stable" becomes "a smoother whose speed is purely a matter of taste" (§5.2).

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
- The two release states drive the reduction to zero (`GainLoop::update_release`); the two ducking states compute the required reduction from the dry levels (§5.2) and drive it there. Nothing else distinguishes them, which is the point of naming them.
- **There is deliberately no voice-onset fast trigger, and no seeding of the reduction on re-entry into a ducking state.** Both were necessary under the old closed loop: its measurement was stale after a gap, so it had to be *told* where to resume from. Feed-forward has nothing stale to compensate for — the dry levels are correct on the very first step back — so the reduction simply climbs at the Attack time constant. Measured on real commentary, deleting the trigger cut the largest single-tick gain movement from 19.4dB to 8.0dB with no loss of onset response. This is one of the clearest wins from the topology change and should not be reintroduced.

### 5.1a Distributing the reduction across the Bed's channels

The gain loop produces **one** reduction figure for the whole Bed, from the whole Bed's loudness. `AutomixEngineConfig::duck_depth` then decides how much of it each *group* of channels receives, as a 0-1 factor: 1.0 takes the reduction in full, 0.0 leaves those channels at unity. The groups are **Front L/R, Center, LFE and Surround L/R**, plus **Top Front L/R / Top Rear L/R** on 5.1.4 and **Side L/R / Top Side L/R** on 7.1.2. They are resolved against the `BedLayout`'s channel map, so the same group always means the same speakers. Left and Right must duck by the same amount or the difference is heard as the stereo image sliding sideways, and the same applies to every other pair. The 2.0 plugin exposes no duck depth at all, since a single pair has nothing to distribute across.

Two things make this a distribution control rather than a second gain stage, and both are load-bearing:

- **The measurement is untouched.** Detectors still read the dry Bed as a whole, so a channel held out of the ducking does not change what the loop decides to do - it only changes what happens to that channel. Letting per-channel depth feed back into the measurement would put the windup the feed-forward redesign removed (§5.2) straight back in.
- **Scaling is linear in dB.** `reduction_ch = reduction * depth`, applied as `10^(-reduction_ch/20)`, so 0.5 means half the *decibels*. Scaling the linear gain instead would make the relationship between channels drift with the size of the reduction, and the control would stop meaning anything fixed.

All factors at 1.0 reproduces the pre-feature behaviour exactly, which is the default. The motivation is that the Bed is not homogeneous: surround channels usually carry atmosphere that was never competing with the commentator, and the LFE carries no intelligibility to protect at all, so ducking them as hard as the front sacrifices the scene for nothing.

`DuckDepth::per_channel_for` resolves the groups to a factor per channel position, against the same map the loudness meters use (`BedLayout::channel_map`), once per config change rather than per sample. That resolution belongs in the core precisely because the mapping is not fixed: position 2 is Center on a 3/5/6-channel Bed but Left Surround on a 4-channel one, so a wrapper exposing positional settings would mislabel them on every layout but the widest.

Applied in `AutomixProcessor::process_bed` at the existing single gain choke point, which now ramps a gain *per channel* (`apply_ramped_gain_per_channel`) and tracks `current_gain` per channel so the channels can sit at different gains without seams between sub-chunks.

### 5.2 Control law: one feed-forward loop

One gain loop, reading the 3 s short-term window on both sides:

```
Bed(dry) ─┬──────────────────────────► ×g ──► Bed out
          │                             ▲
          └──► short-term ──┐           │
                            ├─► required = target − (COM − BED) ─► Attack/Release ─┘
COM(dry) ───► short-term ───┘
```

`GainLoop` (`automix/gain_loop.rs`) is a one-pole follower with separate **Attack** and **Release** time constants (`EnvelopeDetector`), stepped every 64-sample sub-chunk (~1.3 ms at 48 kHz) by `AutomixProcessor::process_bed`. Its readings are O(1) to query (§9a), and the follower is exact for any step size, so the gain follows a smooth curve whatever buffer size the host uses, and a voice-detector state change takes effect at once.

| | default | range (VST3) |
|---|---|---|
| Attack | 2.5 s | 0.1–10 s |
| Release | 3 s | 0.4–12 s |

**Attack is long on purpose**, and this is the single most important tuning fact. A follower that is quicker to grab than to let go sits *above* a fluctuating input: it rides the peaks. So the reduction it applies exceeds what the programme balance needs, by an amount set by how much the bed's level fluctuates, not by how much ducking is called for. With attack close to release the follower averages instead. Measured with `examples/subtle_reduction.rs` on real broadcast programme: 35 min of commentary against a stereo bed, two separate 10-minute sections, COM raised 6 dB so that ~1.9 dB is needed. Rows are ducking time where 1–3 dB was needed:

| Attack (Release 3 s) | mean reduction | RMS around needed | gain travel |
|---|---|---|---|
| 0.15 s | 3.8–4.2 dB | 2.9–3.3 dB | 1.1–1.2 dB/s |
| 1 s | 2.4–3.0 dB | 1.4–1.9 dB | 0.6 dB/s |
| **2.5 s (default)** | **1.6–2.2 dB** | **1.1–1.2 dB** | **0.3–0.4 dB/s** |
| 5 s | 1.1–1.7 dB | 1.0–1.2 dB | 0.2 dB/s |

**The cost is a slow response to a large, sudden need**, typically the commentator starting after a pause, or a bed surge. The 3 s window takes a second or more to register a change, and a 2.5 s attack takes about that long again to follow it. Where 6+ dB was needed, the default applied 2–6.5 dB on average. That is the trade made for a relaxed mix. 1 s is the setting to reach for if onsets are being covered for too long.

**How it got here.** The control law has been a feedback loop, then a three-stage feed-forward cascade, and is now one loop. Each step is worth knowing so it is not reverted:

- **Feedback to feed-forward** (§4): measuring the gained Bed made the control see its own effect three seconds late. Every attempt to tune that topology traded windup and pumping against sluggishness.
- **The cascade.** Three stages added in dB: the slow stage on the 3 s window, plus a mid stage on the 400 ms window (a ±6 dB trim, attack 150 ms / release 600 ms) and a fast stage on a ~50 ms K-weighted detector (a positive-only +4 dB trim, attack 5 ms / release 400 ms). It responded within 0.1 s. But both trim stages were quick to grab and slow to let go, so on a fluctuating bed they peak-followed. A balance needing ~2 dB got 4.1–4.8 dB on average, with the gain travelling 6.6–8.3 dB/s. A dead band ("Tolerance") on the trims and switching the fast stage off each helped. Making the mid stage symmetric did not, because the fast stage filled the gap. Neither removed the cause, and the result was harder to judge by ear than one honest loop. Both trim stages were removed, and the 50 ms detector with them.
- **Reading COM quickly is wrong.** This was tried in the cascade. At short windows a speech signal's "level" is its syllable envelope, so the gain tracked vowels. And a 3 s window over speech-with-gaps reads COM systematically quieter than a 400 ms window during speech, so stages on different windows disagreed by a constant offset. One window on both sides measures one quantity: the programme balance.
- **The control tick is gone.** The loop used to step once per ~100 ms tick, because querying `ebur128`'s windows was expensive. With the windows O(1), stepping per sub-chunk costs nothing, and a tick only made the gain move in 100 ms jumps.

A short lookahead delay ahead of the gain was also implemented, evaluated on real material and removed. A lookahead costs exactly its own length in latency, and keeping the signal path strictly unbuffered preserves the zero-added-latency property the OB-van deployment depends on (§7).

Supporting details:
- **No voice-onset trigger and no seeding.** The feedback design needed one, because after a gap its measurement was stale; seeding the envelope produced an instantaneous ~19 dB jump. Feed-forward has nothing stale to compensate for, so the reduction simply climbs at the Attack time constant. Removing the trigger cut the largest single-tick gain movement from **19.4 dB to 8.0 dB**.
- **The release ease-in and the faster interview recovery** are fixed proportions on top of Release, not separate settings (§5.3).

### 5.3 Timing: why hangover and hold must span a sentence pause

The state machine is only as stable as the booleans driving it. With the original 300 ms VAD hangover and 300 ms hold, measured commentary pauses of ~1.2 s dropped COM to "no voice" in **every inter-sentence break**, flipping to `ReleaseToUnity`, shedding several dB and re-ducking on the next sentence: a swell repeating at sentence rate. Defaults are therefore `VoiceActivityConfig::hangover_seconds = 0.8` and, inside `GainLoop`, a fixed `RELEASE_HOLD_SECONDS = 0.8`. Together, the Bed does not begin recovering until COM has been quiet for ~1.6 s, so natural pauses pass without movement.

**The resistance is an ease-in, not a freeze.** Freezing the gain outright for 0.8 s and then releasing normally is the obvious implementation, and it sounds wrong. It puts a corner in the gain curve, measured at exactly 0.000 dB/tick for 0.8 s and then a step straight to −0.48 dB/tick. A discontinuity in the *rate* of change reads as "nothing happens, then it moves", which is exactly the hold a listener notices. The release time constant is therefore **stretched** by `HOLD_RELEASE_STRETCH` at the instant ducking stops and eased geometrically back to 1× over `RELEASE_HOLD_SECONDS`. Recovery begins immediately but far too slowly to hear, then accelerates; short gaps still recover almost nothing.

Interview passthrough skips the ease-in and recovers `INTERVIEW_RELEASE_SPEEDUP` (4×) faster, since reaching unity promptly is that state's entire purpose.

Both are deliberately **fixed rather than exposed**. They are properties of speech, not matters of taste, and having the hold as a user control ("Hold time") was part of what made the old parameter surface unmanageable.

### 5.4 Live reconfiguration

`AutomixEngine::set_config`/`GainLoop::set_config` replace a config in place without resetting the in-flight reduction or the hold timer. A wrapper can call this every block, cheaply, so host automation (or a dragged slider) actually takes effect. This was added after a real bug: VST3's `initialize()` only ever read its parameters once, at plugin load, so raising `max-gain-reduction-db` mid-session silently had no effect. All wrappers now call `AutomixProcessor::set_config`; the VST3 does so every `process()` call.

**No existing library implements this specific ratio-driven ducking policy**, so `AutomixEngine`/`GainLoop` are bespoke. The underlying `EnvelopeDetector` primitive is generic enough that a small permissively-licensed DSP crate could have supplied it, but it's a few lines and hand-rolling it reads better here.

## 6. Voice-activity detection

**Implemented: real voice-activity detection on *both* the Dialogue (COM) and Bed (IS) sides.** `com_is_assist_core::voice_activity::SileroVad` wraps Silero VAD (a small recurrent ONNX model, vendored in-repo at `crates/com-is-assist-core/assets/silero_vad.onnx` - MIT-licensed, see `docs/LICENSING.md`) run through `ort` (the one place a C++ library - ONNX Runtime itself - sits underneath). Audio is resampled to Silero's required 16kHz mono via `rubato` (pure Rust), fed through in the model's native 512-sample (32ms) chunks with the 64-sample inter-chunk context and recurrent state it expects, and the resulting per-chunk speech probability is smoothed by a hangover timer (`VoiceActivityConfig::hangover_seconds`, **default 800ms**) into a stable `voice_active` boolean. That hangover is effectively "how long a pause still counts as *still talking*", so it has to span a natural **inter-sentence** break, not just an inter-word one — it directly gates the §5 state machine, and at the original 300ms every ~1.2s commentary pause dropped to `ReleaseToUnity` and caused an audible sentence-rate swell (see §5.3).

**Guard band on the COM side.** A detector answers "is this speech?", not "is this speech *on this microphone*". A commentary mic picks up the programme bed as crosstalk, and that spill genuinely is speech — so the VAD quite correctly fires on it, and the Bed then ducks itself against its own bleed. `AutomixEngineConfig::com_gate_lufs` suppresses reported COM voice while COM's own **momentary** loudness sits below it (momentary rather than short-term, so the gate tracks the talker rather than lagging seconds behind). It ships **engaged** at -32 LUFS rather than at the bottom of its range: a gate parked at -70 protects nothing, and this is the failure it exists to prevent. Level is the right discriminator here precisely because spill is reliably far quieter than the commentator, while being indistinguishable from speech by content — which is also why this sits *alongside* the VAD rather than replacing the LUFS-floor proxy the VAD was brought in to kill.

Applied in `AutomixProcessor` (that is where COM's measured level lives), so every wrapper inherits it identically, and the **gated** flag is what both the state machine and the GUI indicator see — a lit LED while the guard band was suppressing detection would be a lie. Default −70 LUFS, effectively open: real programme audio sits far above it, so behaviour is unchanged until it is deliberately raised. Useful settings are typically 20–30 dB below the commentator's own momentary level; measured on real commentary (median −26 LUFS), a −40 LUFS gate removes about 10% of detections, which are the quiet tails and spill.

**Two instances, one per side.** The COM detector gates the ratio/automix loop as before. The **IS detector** (`UI.md`'s "voice detector IS") is fed a **mono downmix of the dry Bed** — Silero is a mono model, and "is anyone speaking anywhere on the Bed" is a whole-Bed question, so averaging the channels is the right reduction. It is deliberately fed the *dry* Bed, before automix gain is applied, so IS detection never depends on how hard the mixer happens to be ducking (which would close an unwanted second feedback path). Together the two booleans select the automix state (§5.1), which is what makes the interview-passthrough and over-voice cases possible at all. Each wrapper owns both instances and passes both into `AutomixProcessor::maybe_run_control_step(com_voice_active, is_voice_active)`. COM fails **open** (treated as always voiced if the detector couldn't be constructed, matching pre-VAD behavior); IS fails **closed** (treated as never voiced), which simply leaves the ordinary duck/release states in play rather than inventing an interview/over-voice state.

**The IS detector can be switched off** (`is-voice-detect` in the VST3, `COMIS_IS_VOICE_DETECT=0` offline). Off is treated exactly like a detector that never finds voice, the same as its fail-closed case: only the ordinary duck and release states remain. Interview passthrough and over-voice become unreachable, and the detector is no longer fed. It is for programme where the Bed's "voice" is not worth protecting, such as crowd chants or sung music, so the faster recovery and the higher over-voice ducking would be wrong. The GUI marks it the same way as COM's guard band, with a cross through the IS lamp, and greys out the over-voice ratio, which then has nothing to do.

That boolean now feeds `MixState::from_vad` directly (§5.1) - it replaces what was previously derived from "is COM's short-term loudness above -70dB LUFS?" A loud non-speech noise (room tone, static, another open mic) no longer gets treated as "the target dialogue is present," which the LUFS floor fundamentally couldn't distinguish. `AutomixProcessor` doesn't own the VAD itself - each wrapper (VST3 and `com-is-assist-offline`) owns its own `SileroVad` instance (fed dialogue audio directly, independent of Bed's own state/caps-negotiation lifecycle) and passes the resulting `voice_active` into `AutomixProcessor::maybe_run_control_step` explicitly, the same way `automix_enabled` already was - keeping `AutomixProcessor`'s own tests decoupled from needing real speech audio to exercise the ratio/gain logic, while every wrapper still shares the exact same detection implementation (no risk of the *algorithm* diverging, even though each holds its own instance). Fails open (treats COM as always voice-active, matching the pre-VAD behavior) if the ONNX Runtime binary couldn't be obtained at startup, rather than silently never ducking at all.

**No denoising.** An `nnnoiseless`/DeepFilterNet path feeding a separate `Cleaned` measurement was part of the original concept and was never built. The VAD alone turned out to answer the question the automix actually asks - "is the commentator talking?" - without it.

## 7. GStreamer element (removed)

The project originally shipped a GStreamer element (`comisassist`, 2 sinks / 3 srcs via `GstCollectPads`) as the primary broadcast deployment target, with the VST3 plugin as the DAW-side validation tool. **It has been removed** - the deployment need it existed for went away, and carrying a second wrapper means carrying a second way for behaviour to diverge. The crate, its spec (`GSTdefinitions.md`), its CI workflow and its `cargo-c` build path all went with it. Git history has the full implementation and the design notes that went with it if it is ever wanted back.

Two things it taught are worth keeping, because they constrain the core rather than that wrapper:

- **Zero added latency is a property of the core, not of a wrapper.** Output latency was deliberately decoupled from the automix control rate: audio is gained and forwarded as it arrives, with the control target recomputed independently of how output happens to be chunked. The first version batched a full tick before emitting anything, which a live pipeline with a real-time sink audibly glitched on. `AutomixProcessor` still works that way and still adds nothing.
- **The control rate is not the loudness rate.** R128 loudness only carries new information at about its 100 ms gating-block rate, which is why the loop once ran on a 100 ms tick. But stepping a smoother and querying O(1) windows is cheap enough to do every sub-chunk, and doing so is what makes the gain move smoothly rather than in 100 ms jumps (§5.2).

## 8. VST3 wrapper design

**`nih-plug`** over JUCE: a real, actively maintained Rust plugin framework targeting VST3 and CLAP directly, with a bundler (`cargo xtask bundle`), GUI adapters (egui/iced/vizia) if ever needed, and sample-accurate automation support. `nih-plug` is less battle-tested than JUCE for VST3 specifically, but that's an acceptable risk. The core-wrapping structure enforces "no DSP/parameter divergence": both wrappers call into the same `com_is_assist_core::automix::AutomixProcessor` (loudness meters, ratio/gain engines, continuous gain ramp — extracted out of what was originally duplicated wrapper logic, once a second wrapper needed the identical control loop) rather than each re-deriving its own copy.

**Bus layout — Bed on the main bus, COM on a mono sidechain, one plugin per bed format.** The plugin originally used one fixed 8-in/8-out bus (Bed 0-5, Dialogue 6, unused 7). It now takes the Bed as the main input and output and the COM as a mono VST3 aux (sidechain) input. One binary exports four plugins: 2.0 (2ch), 5.1 (6ch), 5.1.4 (10ch, `k51_4`) and 7.1.2 (10ch, `k71_2`). Each has exactly one fixed layout, instead of one plugin with several host-negotiated layouts. Each plugin reports and accepts exactly one VST3 speaker arrangement (`kStereo` / `k51` / `k51_4` / `k71_2`, sidechain `kMono`), which requires the patched nih-plug in `vendor/nih_plug`. See `Specs/vstDefinitions.md` "Bus structure" for the reasoning. In the core, the layout is a `BedLayout` rather than a channel count, because 5.1.4 and 7.1.2 are both ten channels; `bed_channel_map(count)` still exists and reads 10 as 5.1.4. The channel positions follow ITU-R BS.2051: 5.1.4 heights at U±030 / U±110, 7.1.2 sides at M±090, its Ls/Rs at M±135 and its top pair at U±090. BS.1770 weights elevated channels at 1.0 and gives the +1.5 dB surround weight to the 7.1.2 side pair.

A `mix-dialogue-to-bed` bool parameter (the "bus insert mode" toggle) controls whether the main output bus is leveled Bed alone (for inserting directly on a Bed/IS channel strip) or have Dialogue mixed in (matching `mix_src` — for standalone use as the final program bus). When mixed in, Dialogue is restricted to Left/Right/Center only — it never reaches the LFE, surrounds, sides or heights — placed via a `divergence` parameter (`UI.md`'s "Voice divergence": 0% = Center-only, 100% = split Left/Right) using `com_is_assist_core::automix::mix_dialogue_into_bed`'s equal-power pan law, extracted as a shared function for the same "no DSP divergence" reason as `AutomixProcessor`. On 2.0 there is no Center, so Dialogue is split equally between L and R and divergence does not apply. Dialogue is not passed through on its own: it arrives on the input-only sidechain, and already has its own path in the host. A separate `bypass` bool parameter disables automix entirely (Bed passes through at unity gain) when `true` — named for the conventional meaning of a "Bypass" control.

**Control surface.** The three-stage cascade removed five timing parameters (`adaptation-ms`, `attack-ms`, `hold-ms`, `release-ms`, `interview-release-ms`) because its ballistics were emergent, and replaced them with one `speed` macro. With the cascade reduced to a single loop (§5.2), timing is simply that loop's **`attack-ms`** and **`release-s`**. The hold and interview recovery stay fixed (§5.3). Alongside them is **`is-voice-detect`**, which switches the Bed-side detector off (§6). The full VST3 surface is `target-ratio`, `overvoice-ratio`, `max-gain-reduction-db`, `com-gate-lufs`, `is-voice-detect`, `attack-ms`, `release-s`, `bypass`, `mix-dialogue-to-bed`, `divergence`, plus the duck-depth groups of the format.

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

**The loudness query was the biggest single cost and the least obvious.** `libebur128` recomputes a window's sum of squares on every call rather than maintaining it: measured at ~700us for short-term and ~100us for momentary on a 6-channel bed, and the control step of the time asked three times per tick. `KWeightedLoudnessMeter` now keeps the *same rectangular windows* as running totals, adding the newest millisecond of energy and subtracting the one ageing out - O(1) per query. The window shape is deliberately unchanged, so earlier ear-tuning carries over; an exponential average would have been simpler and was rejected for exactly that reason. Agreement with `ebur128` is asserted on steady tones (<0.1 dB), through repeated level steps (<0.5 dB), and measured at **0.043 dB** across real programme material (`cargo run --release --example compare_meters -- <file.wav>`).

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

Two of these were the same mistake in different places: a value that only changes at the ~100ms control tick being consulted by code that runs per audio sub-chunk. (The tick has since been removed altogether: the gain loop steps every sub-chunk, §5.2.) The tick is 100ms because that is how often **R128 loudness** carries new information; the VAD and the state machine have something new to say far sooner, and should not have been quantised to it. `dialogue_reference_lufs` was a third instance, cached at tick rate purely because querying `ebur128` was expensive (§9a) - a reason that no longer existed.

**The remaining ~49ms is Silero's**, and cannot be optimised away: the model consumes 512 samples at 16kHz, so no answer exists until 32ms of audio has arrived, plus however long it takes to commit (the slowest onsets measured ~209ms on soft speech). Going below that needs look-ahead, which is **not** implemented and was rejected rather than deferred:

- Delaying only the Bed desynchronises it from picture. The Bed is the match sound; a 40ms offset against video is a lip-sync error.
- Delaying everything puts latency in the commentator's own monitor path, which is the one place a live operator will not accept it.
- Zero added latency is otherwise a property of the whole signal path (§7), relied on in live use.

## 10. Testing and validation

The numbered items below are what the design is actually held to; 3 and 4 run on every change. Two `examples/` in the core are the real-time tooling: `bench_realtime` reports per-block cost against the budget by percentile (the *worst* block is what a host is killed by, not the mean), and `compare_meters` checks the incremental loudness windows against `ebur128` on a real file.

1. **Golden-reference comparison against the JSFX meter** — now a *ballpark sanity check*, not a bit-exact port validation, since §2 documents two deliberate standards-conformance deviations (surround weight, no sqrt(2) mono hack). Add a debug-logging JSFX fork (`tests/golden/reaper/..._LOGGING.jsfx`, never touching the real reference file) dumping per-tick LUFS/ratio values to CSV, and compare against `com-is-assist-offline`'s output with a tolerance wide enough to absorb the known, intentional deltas (document the expected delta magnitude somewhere durable so a widening tolerance isn't mistaken for a bug later). Because the project is open source, all fixtures used here must be synthetic or freely-licensed audio — not real SRF broadcast archive material, which can still be used for local/internal validation but never committed to the public repo.
2. **Conformance testing "for free"**: since the `ebur128` crate is already validated against the official EBU Tech 3341 test vectors, running those same public vectors through `Ebur128Meter` gives a real standards-conformance check that the original JSFX never had — worth adding as its own `cargo test` target independent of the JSFX comparison.
3. **Unit tests (`cargo test`)**: `Ebur128Meter` against known-level synthetic test tones (expected LUFS within tolerance: a 400 Hz full-scale sine measures ≈-3.7 LUFS), `KWeightedLoudnessMeter`'s windows agreeing with `Ebur128Meter` on steady tones and through level changes, `MixState`'s full truth table, and `AutomixEngine`'s feed-forward exactness, exact Attack/Release time constants, release monotonicity and ease-in.
   - **Duck depth** (`tests/duck_depth.rs`) drives the real `AutomixProcessor` rather than the gain helper, since the property worth protecting is end-to-end: the depth a wrapper sets is the depth the audio receives, in dB, after the ramp and the cascade have had their say. Two of the five are about the grouping rather than the gain - that `DuckDepth::per_channel` follows the *channel map* across all five supported channel counts (position 2 is Center on a 3/5/6-channel Bed but Left Surround on a 4-channel one), and that the front and surround pairs can never diverge, which is the guarantee against hearing the stereo image slide sideways.
   - **Shipped defaults are pinned** (`com-is-assist-vst3`'s own unit tests): one test asserts every default outright, another checks the VST3's against `AutomixEngineConfig::default()` so the two wrappers cannot disagree about out-of-the-box behaviour. These exist because the defaults have moved more than once, and because a value on a plugin's GUI gives no clue whether it is the shipped default or something the host restored from a saved session - so it is a question the plugin cannot answer by inspection.
   - **Two tests specifically guard against the pumping regression** (§5.2), since it is the defect this design exists to fix and it would be easy to reintroduce: one asserts the settled reduction is *exactly* what the dry levels imply (`target − (COM − BED)`), which is only a well-defined question at all because the path is feed-forward; another asserts a silence-sentinel reading holds rather than driving the gain. Further tests assert that Attack is the loop's exact time constant, that Attack and Release each govern their own direction and leave the other alone, that re-engagement after a gap climbs continuously, that release never *adds* reduction, and that release eases in rather than stepping — the last two being real defects found by listening, not hypotheticals.
4. **Empirical pumping measurement**: `com-is-assist-offline` emits per-tick CSV - `tick,time_s,bed_lufs_m,com_lufs_m,com_voice_active,is_voice_active,mix_state,ratio_lu,target_lu,gain_reduction_db`, one row per 100 ms (the loop itself steps every sub-chunk). Config is overridable from the environment (`COMIS_TARGET_RATIO_LU`, `COMIS_OVERVOICE_RATIO_LU`, `COMIS_MAX_GAIN_REDUCTION_DB`, `COMIS_COM_GATE_LUFS`, `COMIS_ATTACK_SECONDS`, `COMIS_RELEASE_SECONDS`, `COMIS_IS_VOICE_DETECT` (0 = off), `COMIS_DUCK_DEPTH` - up to eight comma-separated factors in the order `front,center,lfe,surround,side,top_front,top_rear,top_side` (`side`/`top_side` act on 7.1.2 only, `top_front`/`top_rear` on 5.1.4 only), blank fields keeping their default - `COMIS_BED_LAYOUT=7.1.2` to read a 10-channel Bed as 7.1.2 rather than 5.1.4, and `COMIS_VAD_HANGOVER_SECONDS`) so a sweep needs no rebuild. The §5.2 table comes from `examples/subtle_reduction.rs` instead, which runs real material through `AutomixProcessor` directly and buckets the result by the needed reduction; re-run it after any change to the control law or the timing defaults.

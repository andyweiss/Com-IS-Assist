# Gstreamer definitions

GStreamer element (`GstComISAssist`) pad/data definitions (see `Specs/TechnicalConcept.md` section 7). Unlike the VST3 side, GStreamer caps are plain integer channel counts, not named speaker arrangements — but the Bed channel count is still capped at 6 (5.1), matching the VST3 side, to keep BS.1770 weighting within its standard, well-defined channel map. Everything below is GStreamer-API-level (pads/caps/`GstMeta`/bus messages) and unaffected by the project's C++→Rust migration — the element is implemented via `gstreamer-rs`, but its external pad/data contract is unchanged.

## Pads

### Sink pads (inputs)

| Pad | Channels | Notes |
|---|---|---|
| `dialogue_sink` | 1 (fixed, mono) | COM signal |
| `bed_sink` | 2–6 (max 5.1) | IS signal — capped at 6 channels, same ceiling as the VST3 side, even though GStreamer's plain integer caps (`channels=[2,6]`) could technically go higher; keeps BS.1770 channel weighting within the standard L/R/C/LFE/Ls/Rs map with no undefined behavior past 5.1 |

### Src pads (outputs)

| Pad | Channels | Notes |
|---|---|---|
| `mix_src` | mirrors `bed_sink`'s negotiated channel count | leveled (ducked) Bed + Dialogue summed |
| `is_leveled_src` | mirrors `bed_sink`'s negotiated channel count | leveled (ducked) Bed only, Dialogue not folded in |
| `dialogue_src` | 1 (fixed, mono) | Pristine passthrough of the Dialogue input — never touched by automix or denoise, same contract as the VST3 side's Dialogue-out bus |

`mix_src` and `is_leveled_src` always mirror `bed_sink`'s negotiated channel count — same "Bed-in and Bed-out arrangement must match" constraint as the VST3 side. Three src pads total, confirmed for parity with VST3's Dialogue-out bus.

## Analysis data exposure

Per `UI.md`'s Display section (IS/Com LUFS momentary + integrated, ratio, voice-activity LEDs for both Comm *and* IS, current gain reduction), this data needs to reach two different audiences, so two complementary mechanisms:

1. **Carried to the next plugin in the chain** — attach a custom `GstMeta` (e.g. `GstComISAssistAnalysisMeta`) to every buffer pushed on both src pads, carrying: `is_lufs_m`, `is_lufs_s`, `is_lufs_i`, `com_lufs_m`, `com_lufs_s`, `com_lufs_i`, `ratio_lu`, `gain_reduction_db`, `voice_active_com`, `voice_active_is`. Downstream GStreamer elements (another Com-IS-Assist element, a logger, a metering/graphics element) read this in-band, per-buffer, with no bus subscription needed. This is new — `TechnicalConcept.md` section 7 only specified the mechanism below.
2. **Exposed to the console** — the already-specified periodic (~10 Hz) `"com-is-assist-stats"` bus message carries the same fields, plus the same values as read-only GObject properties for simple polling. Bridging those out to whatever protocol a physical console/monitoring system actually speaks (OSC/EmBER+/SNMP/...) is a controlling application's job, outside this element's scope.

`voice_active_is` is now implemented: a *second*, independent `SileroVad` instance runs on a mono downmix of the dry Bed (see `TechnicalConcept.md` section 6). Together with the COM detector it selects the automix state (section 5.1), which is what drives both the interview-passthrough and over-voice behaviors.

## Properties

The five former timing properties (`attack-seconds`, `hold-seconds`, `release-seconds`, `interview-release-seconds`, `adaptation-seconds`) and `tolerance` were **removed** by the multi-loop redesign: the cascade's ballistics are emergent, so there was nothing meaningful left for them to set (`TechnicalConcept.md` §5.4). This is a deliberate breaking property change.

Settable (all live — `set_property` forwards to `AutomixProcessor::set_config` without resetting accumulated state):

| Property | Type | Default | Notes |
|---|---|---|---|
| `target-ratio` | double (LU) | 5.0 | Distance required while COM alone has voice |
| `overvoice-ratio` | double (LU) | 13.0 | **Absolute** distance required while COM *and* IS both have voice |
| `speed` | double | 1.0 | **The one timing control** - scales every loop stage's ballistics together (0.25-8.0, default 1.0). Raise if sluggish, lower if it breathes |
| `max-gain-reduction-db` | double (dB) | 24.0 | |
| `com-gate-lufs` | double (LUFS) | -32.0 | Guard band on COM voice detection: reported speech is ignored while COM's momentary loudness is below this, so bed crosstalk in the commentary mic cannot duck the bed against itself. The default is engaged, not parked at the bottom of its range |
| `automix-enable` | boolean | true | Note the polarity is inverted from VST3's `bypass` |
| `divergence` | double (%) | 0.0 | |
| `duck-depth-front` | double | 1.0 | Share of the computed gain reduction taken by the front Left/Right pair |
| `duck-depth-center` | double | 1.0 | ... by the Center channel |
| `duck-depth-lfe` | double | 1.0 | ... by the LFE channel |
| `duck-depth-surround` | double | 1.0 | ... by the surround pair |

### Duck depth per bed channel group

One gain reduction is computed for the whole Bed, as before; these four decide how much of it each group of channels actually takes. `1.0` is the full figure (6 dB of ducking takes 6 dB off those channels), `0.0` leaves them at unity - the `1:1` .. `1:0` ratio the VST3 displays. Values scale the **decibels** linearly, so 0.5 against 6 dB is 3 dB, which keeps the relationship between groups fixed at every reduction depth instead of drifting with it. All four at 1.0 is the previous behaviour exactly.

It exists because the Bed is not homogeneous: ducking the surrounds as hard as the front throws away atmosphere that was never competing with the commentator, and the LFE carries no intelligibility to protect at all.

**Grouped, not per channel.** Left and Right must duck by the same amount or the difference is heard as the stereo image sliding sideways, and the same applies to the surround pair; independent settings make that mistake easy to reach and buy nothing.

The groups are resolved to actual channels by the core (`DuckDepth::per_channel`) against the negotiated channel count, so the names hold on every layout:

| Channels | Front L/R | Center | LFE | Surround L/R |
|---|---|---|---|---|
| 2 | idx 0,1 | - | - | - |
| 3 | idx 0,1 | idx 2 | - | - |
| 4 | idx 0,1 | - | - | idx 2,3 |
| 5 | idx 0,1 | idx 2 | - | idx 3,4 |
| 6 | idx 0,1 | idx 2 | idx 3 | idx 4,5 |

A group with no channels on the current layout simply does nothing. An earlier revision exposed six *positional* properties instead; that was dropped because position 2 is Center on a 3/5/6-channel Bed but Left Surround on a 4-channel one, so the names were wrong on exactly the layouts where it mattered.

Read-only:

| Property | Type | Notes |
|---|---|---|
| `current-gain-reduction-db` | double | |
| `current-ratio-lu` | double | |
| `voice-active` | boolean | COM-side VAD, **after** the `com-gate-lufs` guard band - i.e. what actually drives the mix |
| `voice-active-is` | boolean | IS-side VAD |
| `current-mix-state` | string | One of `release`, `duck-target`, `interview`, `duck-overvoice` |


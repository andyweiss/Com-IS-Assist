# VST definitions

VST3 bus/parameter definitions for the Com-IS-Assist plugin, built with `nih-plug` (see `Specs/TechnicalConcept.md` section 8). Implemented in `crates/com-is-assist-vst3`.

## Bus structure

**A single fixed 8-channel bus, both input and output** — not the multi-bus layout originally sketched below this line. `nih-plug`'s bus model is "one main input + one main output, plus anonymous aux ports," which doesn't map cleanly onto a named-multi-bus VST3 layout (this was flagged as an open item before implementation; resolved by not doing multi-bus at all, sidestepping both the mapping question and the multi-output-bus host-compatibility risk).

| Channel (0-indexed) | Role | Notes |
|---|---|---|
| 0 | Bed Left | Fixed 6-channel Bed layout (not the dynamic 2-6ch range the GStreamer element supports): Left, Right, Center, LFE (unused), LeftSurround, RightSurround |
| 1 | Bed Right | |
| 2 | Bed Center | |
| 3 | Bed LFE | Unused for Dialogue mixing - passes through leveled Bed content only |
| 4 | Bed LeftSurround | |
| 5 | Bed RightSurround | |
| 6 | Dialogue | Mono, always dry |
| 7 | Unused | Always silenced on output, regardless of what the host sends on input |

### `mix-dialogue-to-bed` parameter ("bus insert mode")

A bool parameter selecting what channels 0-5 carry on output:

- **On** (default): channels 0-2 (Left/Right/Center) carry leveled Bed + Dialogue mixed in via the `divergence` pan law below; channels 3-5 (LFE/surrounds) are leveled Bed only, Dialogue never reaches them — matches the GStreamer element's `mix_src`, restricted to L/R/C.
- **Off**: all of channels 0-5 are leveled Bed alone, Dialogue not folded in anywhere — matches `is_leveled_src`, for inserting directly on a Bed/IS channel strip where Dialogue is summed in separately downstream.

Channel 6 (Dialogue passthrough) is unaffected by this toggle either way.

### `divergence` parameter ("Voice divergence", `UI.md`)

0-100%, only has an effect while `mix-dialogue-to-bed` is on. Controls how Dialogue is placed across Left/Center/Right when mixed in, via `com_is_assist_core::automix::mix_dialogue_into_bed`'s equal-power (constant-power) pan law:

- **0%**: Dialogue is Center-only.
- **100%**: Dialogue is split across Left/Right only, each at `1/sqrt(2)` (~-3dB) so the total acoustic power matches Center-only rather than growing louder as it spreads — the standard constant-power crossfade, not a linear one.

## Parameters

- `target-ratio` — LU. The Com/IS distance required while COM alone has voice (`MixState::DuckToTarget`).
- `overvoice-ratio` — LU, 0-24, default 8. The **absolute** (not additive) distance required while COM *and* IS both have voice (`MixState::DuckToOvervoice`) — the double-talk case needs more headroom for COM to stay intelligible. See `Specs/TechnicalConcept.md` section 5.1.
- `max-gain-reduction-db` — dB
- `speed` ("Speed") — 0.25x-4x, default 1x. **The one timing control.** Scales every loop stage's ballistics together; raise it if the mixer feels sluggish, lower it if it breathes. It replaces the five separate time parameters earlier designs needed, which the multi-loop cascade made meaningless: its effective attack and release are emergent (see `Specs/TechnicalConcept.md` section 5.4).
- `lookahead-ms` ("Lookahead") — 0-20ms, **default 0**. Non-zero delays both Bed and Dialogue equally (so the Mix stays aligned) while the detectors tap the signal before the delay, letting the fast stage act on a transient before it reaches the output. The plugin reports the delay to the host via `set_latency_samples`; at the default of 0 it is a true no-op and reported latency stays 0.
- `interview-passthrough` — bool, **default off**. Enables the fast-recovery IS-only state at all; with it off, voice on IS alone behaves as an ordinary release. Off by default because a loud PA/stadium announcement also reads as "voice on IS" and recovering the Bed fast on that is risky on air.
- `bypass` — `true` disables automix entirely (Bed passes through at unity gain); `false` (default) is normal operation. Inverted polarity from the GStreamer element's `automix-enable` (`true` = active) — a deliberate wrapper-specific naming choice, matching the conventional meaning of a "Bypass" control in a DAW plugin.
- `mix-dialogue-to-bed`, `divergence` — see above

**Not exposed anywhere**: the per-stage time constants and authorities of the three loop stages, and the release hold. These are properties of speech and of the R128 window structure rather than matters of taste - see `Specs/TechnicalConcept.md` sections 5.2/5.3. The `speed` macro scales the former as a group. Real voice-activity detection is implemented on **both** sides (`com_is_assist_core::voice_activity::SileroVad`, see `Specs/TechnicalConcept.md` section 6) but always-on: neither detector has an on/off parameter. The IS detector is fed a mono downmix of the dry Bed, and the behavior it unlocks is gated by `interview-passthrough`/`overvoice-ratio` rather than by disabling detection.

**Live updates**: `process()` calls `AutomixProcessor::set_config()` every block with the current live values of every parameter above, so changing any of them (a host automation lane, or dragging the GUI's `ParamSlider`s) takes effect immediately. Fixes a real bug: `initialize()` only reads these parameters once, at plugin load - without the per-block refresh, raising `max-gain-reduction-db` past its 24dB default toward its 48dB ceiling mid-session had no effect, since `AutomixEngine` kept using whatever config it was constructed with. See `Specs/TechnicalConcept.md` section 5.

## GUI (custom, meters + controls)

Resolved: `nih-plug`'s parameter setter (`ParamMut`) is deliberately `pub(crate)` - a plugin cannot programmatically update its own parameter's displayed value from `process()`, so live meters can only be shown via a custom GUI reading shared atomic state directly. Once a GUI existed for that reason, it also grew to expose the actual parameter controls - see `Specs/TechnicalConcept.md` section 8/11 for why the original "meters only, no GUI controls" scoping was dropped.

Implemented via `nih_plug_egui` (`create_egui_editor`):

**Overall layout**: a tall, professional-bargraph-style meter bank on the left, all parameter controls in a single column on the right (`ui.horizontal` splitting a left `ui.vertical` meter bank from a right `ui.vertical` controls panel, separated by `ui.separator()`) - modeled loosely on the original Reaper JSFX reference meter and professional loudness-meter hardware (e.g. RTW's product line), not on any specific peak-meter ballistics standard: this is a **look-and-scale-only** redesign, the underlying R128/EBU loudness measurement, DSP, and value semantics are unchanged from the description below.

**Meters** - a `Meters` struct (four `Arc<AtomicF32>` fields) is written from `process()` (only while the editor is actually open, to keep the audio thread's hot path clear the rest of the time) and read from the `editor()` closure each GUI frame. Each bar is `METER_BAR_HEIGHT` (420px) tall and narrow (`METER_BAR_WIDTH`, 20px), with a fixed-pitch segmented-gap overlay (`draw_meter_segments`, background-colored hlines at `METER_SEGMENT_PITCH` across the fill) giving every bar the discrete-LED-bargraph look of a professional meter, purely cosmetic and independent of the actual fill-fraction math. Layout, left to right: gain reduction (+ its own scale), IS (+ its own scale), ratio, COM (+ its own scale) - gain reduction sits immediately next to IS/ratio specifically so the two values the user tunes by ear against each other are easy to read side by side. Each loudness bar gets its own scale immediately to its right, as close as the tick text allows, rather than one shared scale off to a single side. Each meter column is a fixed width (`fixed_width_column`, content centered within it) so a column never changes width frame-to-frame as its live numeric label's text length changes (e.g. "-inf LUFS" vs "-6.3 LUFS") - without that, later columns would visibly shift left/right as values changed. `ui.spacing_mut().item_spacing.x = 0.0` on the row disables egui's automatic inter-widget spacing so every gap in the row is exactly what's asked for via `ui.add_space`, not that plus an extra unrequested default gap. All bar meters share the same visual language (a filled rect over a dark background rect, drawn directly via `egui::Painter`):
- **Gain reduction**: a standalone bar (`gain_reduction_meter`), fills top-down from 0 to `applied_gain_reduction_db()` against a fixed 0-48dB range (`GAIN_REDUCTION_METER_MAX_DB`, deliberately more headroom than the 24dB default ceiling so a raised `max-gain-reduction-db` ceiling stays fully visible), red, with its own top-down scale (`gain_reduction_scale_column`) and a `-{reduction:.1} dB` label below. Standalone rather than carved out of the IS bar (an earlier iteration's approach) so both gain reduction and ratio can be read clearly at a glance while tuning, per explicit user request.
- **IS (Bed)**: fills bottom-up (the conventional level-meter orientation) to `bed_momentary_lufs()` (pre-gain, via a second `Ebur128Meter` kept separate from the closed-loop control meter, on a fixed -60..0 LUFS display range), cyan, with the pre-gain `{lufs:.1} LUFS` reading below.
- **Ratio**: positioned between the IS and COM bars, matching the original JSFX reference meter's layout. Rather than an independent bar with its own arbitrary range, it's drawn as a bar spanning the gap between Bed's *effective* (post-reduction) level and that same level plus `display_ratio_lu` on the *same* shared LUFS scale as the loudness bars (LU is literally a difference of LUFS values, so this is a valid, not just visually convenient, choice), plus a `{lu:+.1} LU` label. **Uses `AutomixProcessor::display_ratio_lu()` for the bar's height/label, *not* `applied_ratio_lu()`** - the two are computed from different meters and time-constants (`display_ratio_lu` is dialogue momentary minus Bed's *effective* momentary - `bed_effective_momentary_lufs()`, the pre-gain reading minus the currently-applied reduction - matching what's actually audible; `applied_ratio_lu` is short-term and strictly post-gain via a different meter, the value that actually drives automix decisions) - using `applied_ratio_lu` here, or anchoring against Bed's raw pre-gain level, made the bar track the IS bar's fill correctly but not the COM bar or the actual post-reduction level, since neither was computed from what's actually audible. `display_ratio_lu` is also floored at `0.0`: a negative "distance" isn't meaningful to show here (COM briefly dipping below Bed's effective level during normal speech gaps isn't itself alarming). **The bar's *color* is a 4-state at-a-glance read of where `applied_ratio_lu()` (the real, signed control-loop value - deliberately not the floored display value, which can never be negative, so the grey state would be unreachable) sits relative to `target-ratio`**: grey when negative (COM currently *quieter* than IS - a distinct situation from "not loud enough yet"), red when positive but under target by more than `RATIO_METER_GREEN_BAND_LU` (2 LU), green within +/-2 LU of target (on target), orange above that band (COM louder than needed - excess headroom). The colour is keyed against **whichever target is currently active** (`AutomixProcessor::active_target_ratio_lu`), so the over-voice state is judged against its own higher target rather than appearing permanently "too loud". This is a GUI-only readability threshold and governs nothing in the DSP.
- **COM (Dialogue) momentary loudness**: fills bottom-up like IS, orange, via `AutomixProcessor::dialogue_momentary_lufs()` (never gained, so no pre/post distinction on this side) - plus a `{lufs:.1} LUFS` label (or "-inf" at/below `Ebur128Meter::NEGATIVE_INFINITY_DB`).
- **Scale**: a vertical axis (tick marks + numbers) drawn immediately to the right of the gain-reduction bar, the IS bar, and the COM bar (Ratio doesn't get one, since it shares the loudness bars' scale visually by spanning between them). Loudness scales (`loudness_scale_column`) run bottom-up in 5 LUFS steps; the gain-reduction scale (`gain_reduction_scale_column`) runs top-down in 6dB steps (`vertical_scale`'s `top_down` parameter controls tick direction). Each scale's allocated rect is sized to actually contain the drawn tick text, not just the tick line, so egui's layout system - which has no idea what a `Painter` draws outside the rect it was told about - correctly reserves space for it and neighboring columns don't crowd it. The topmost/bottommost labels are inset slightly so they stay fully visible and centered on their tick rather than clipping at the edge of the drawn area.
- **Voice-activity LEDs**: a small green/gray LED (`voice_activity_led`) under the COM bar reflecting `Meters::voice_active`, and a second one under the IS bar reflecting `Meters::is_voice_active` - lit when the respective Silero VAD instance currently detects speech (see `Specs/TechnicalConcept.md` section 6). Each sits directly under the bar it describes rather than in a shared row, so it reads as a property of that signal.
- **Loop readout**: a line showing what each cascade stage is contributing (`slow`, `mid`, `fast`, in dB), fed by `Meters::slow_db`/`mid_db`/`fast_db`. The three stages have visibly different characters, so the split is the quickest way to tell *which* of them is responsible for what you are hearing.
- **State readout**: a colored text line in the controls column naming the current `MixState` ("Release", "Duck -> target", "Interview passthrough", "Duck -> over-voice"), fed by `Meters::mix_state` (an `AtomicU8` discriminant). Since the mixer's behavior is now defined entirely by which of four states it is in, showing that state directly is the fastest answer to "why is it doing that right now?" - far more legible than inferring it from the meters.

Integrated LUFS (`AutomixProcessor::bed_integrated_lufs`/`dialogue_integrated_lufs` from an earlier iteration of this GUI) was dropped as a deliberate simplification, not a gap - momentary is what the automix control loop and this display both care about; `UI.md`'s "IS LUFS-I"/"Comm LUFS-I" wishlist items are considered out of scope unless a concrete need for them comes up later.

**Layout padding**: the meter bank, the title, and the controls section all share a consistent 12px left indent so nothing sits flush against the window's left edge; the window content also has top and bottom breathing room (`ui.add_space` before the title and after the last control row). Window size (`EguiState::from_size` / `ResizableWindow::min_size`) was increased to 700x560+ minimum to fit the taller bars and the side-by-side meters/controls split.

**Controls** - every parameter from the "Parameters" section above is exposed directly in the same window, still routed through `ParamSetter` (so host automation/undo/preset-recall works identically to the host's own generic parameter UI):
- `target-ratio`, `overvoice-ratio`, `max-gain-reduction-db`, `speed`, `lookahead-ms`, `divergence` - via `nih_plug_egui::widgets::ParamSlider`.
- `bypass`, `mix-dialogue-to-bed`, `interview-passthrough` - via a plain `ui.checkbox` bound through `ParamSetter::begin_set_parameter`/`set_parameter`/`end_set_parameter`, since `nih_plug_egui` has no built-in bool-parameter widget (`ParamSlider` only handles continuous/stepped ranges).

## Building

`nih-plug` doesn't publish to crates.io - it's a git dependency. Bundling uses a workspace `xtask` crate (wrapping `nih_plug_xtask::main()`) plus a `.cargo/config.toml` alias:

```shell
cargo xtask bundle com-is-assist-vst3 --release
```

Produces `target/bundled/Com-IS-Assist.vst3`.

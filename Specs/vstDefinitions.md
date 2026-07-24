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

- `target-ratio` — LU
- `max-gain-reduction-db` — dB
- `attack-ms` ("Fade down time"), `hold-ms`, `release-ms` ("Recovery time") — milliseconds, range 1-5000ms, 1ms step (changed from the GStreamer element's seconds-based equivalents specifically per this wrapper's UI - see `Specs/TechnicalConcept.md` section 8)
- `bypass` — `true` disables automix entirely (Bed passes through at unity gain); `false` (default) is normal operation. Inverted polarity from the GStreamer element's `automix-enable` (`true` = active) — a deliberate wrapper-specific naming choice, matching the conventional meaning of a "Bypass" control in a DAW plugin.
- `mix-dialogue-to-bed`, `divergence` — see above

**Not exposed here, unlike the GStreamer element**: `max-tolerance`/`min-tolerance` (fixed at `AutomixEngineConfig::default()`'s values, not user-adjustable in this wrapper - a deliberate simplification of the VST3 parameter surface). VAD-related parameters (`UI.md`'s "voice detector comm/IS") are M3 scope, not yet implemented in either wrapper.

**Live updates**: `process()` calls `AutomixProcessor::set_config()` every block with the current live values of `target-ratio`/`max-gain-reduction-db`/`attack-ms`/`hold-ms`/`release-ms`, so changing any of them (a host automation lane, or dragging the GUI's `ParamSlider`s) takes effect immediately. Fixes a real bug: `initialize()` only reads these parameters once, at plugin load - without the per-block refresh, raising `max-gain-reduction-db` past its 24dB default toward its 48dB ceiling mid-session had no effect, since `AutomixEngine` kept using whatever config it was constructed with. See `Specs/TechnicalConcept.md` section 5.

## GUI (custom, meters + controls)

Resolved: `nih-plug`'s parameter setter (`ParamMut`) is deliberately `pub(crate)` - a plugin cannot programmatically update its own parameter's displayed value from `process()`, so live meters can only be shown via a custom GUI reading shared atomic state directly. Once a GUI existed for that reason, it also grew to expose the actual parameter controls - see `Specs/TechnicalConcept.md` section 8/11 for why the original "meters only, no GUI controls" scoping was dropped.

Implemented via `nih_plug_egui` (`create_egui_editor`):

**Meters** - a `Meters` struct (four `Arc<AtomicF32>` fields) is written from `process()` (only while the editor is actually open, to keep the audio thread's hot path clear the rest of the time) and read from the `editor()` closure each GUI frame. Layout, left to right (with a small leading space so the bank isn't flush against the window edge): IS (+ its own scale), ratio, COM (+ its own scale) - each loudness bar gets its own scale immediately to its right, as close as the tick text allows, rather than one shared scale off to a single side. Each meter column is a fixed width (`fixed_width_column`, content centered within it) so a column never changes width frame-to-frame as its live numeric label's text length changes (e.g. "-inf LUFS" vs "-6.3 LUFS") - without that, later columns would visibly shift left/right as values changed. `ui.spacing_mut().item_spacing.x = 0.0` on the row disables egui's automatic inter-widget spacing so every gap in the row is exactly what's asked for via `ui.add_space`, not that plus an extra unrequested default gap. All bar meters share the same visual language (a filled rect over a dark background rect, drawn directly via `egui::Painter`):
- **IS (Bed)**: fills bottom-up (the conventional level-meter orientation) to `bed_momentary_lufs()` (pre-gain, via a second `Ebur128Meter` kept separate from the closed-loop control meter, on a fixed -60..0 LUFS display range), cyan - **with the currently-applied gain reduction (`applied_gain_reduction_db()`) carved out as a red segment at the *top* of that same fill, extending down by the reduction amount** (1dB of reduction = 1 LU on the same scale, so this is exact, not approximate). This replaced an earlier separate, standalone gain-reduction meter: showing "how much is being cut" as part of the bar it's cut *from* is far more direct than a side-by-side meter with its own independent scale - the boundary between the cyan and red portions of the bar *is* the effective, audible Bed level. Below the bar: the pre-gain `{lufs:.1} LUFS` reading and a `-{reduction:.1} dB` label for the cut.
- **Ratio**: positioned between the IS and COM bars, matching the original JSFX reference meter's layout. Rather than an independent bar with its own arbitrary range, it's drawn as a bar spanning the gap between Bed's *effective* (post-reduction) level and that same level plus `display_ratio_lu` on the *same* shared LUFS scale as the loudness bars (LU is literally a difference of LUFS values, so this is a valid, not just visually convenient, choice), plus a `{lu:+.1} LU` label. **Uses `AutomixProcessor::display_ratio_lu()` for the bar's height/label, *not* `applied_ratio_lu()`** - the two are computed from different meters and time-constants (`display_ratio_lu` is dialogue momentary minus Bed's *effective* momentary - `bed_effective_momentary_lufs()`, the pre-gain reading minus the currently-applied reduction - matching what's actually audible and what the IS bar's cyan/red boundary shows; `applied_ratio_lu` is short-term and strictly post-gain via a different meter, the value that actually drives automix decisions) - using `applied_ratio_lu` here, or anchoring against Bed's raw pre-gain level, made the bar track the IS bar's fill correctly but not the COM bar or the actual post-reduction level, since neither was computed from what's actually audible. `display_ratio_lu` is also floored at `0.0`: a negative "distance" isn't meaningful to show here (COM briefly dipping below Bed's effective level during normal speech gaps isn't itself alarming). **Colored red when `applied_ratio_lu()` (the real, signed control-loop value - deliberately not the floored display value, which would make the tolerance band's lower bound unreachable) falls outside `[target-ratio - min_tolerance_lu, target-ratio + max_tolerance_lu]`** - the same dead-band `AutomixEngine::process_tick` itself uses to decide whether Bed's gain needs adjusting (read from `AutomixEngineConfig::default()`, since `max-tolerance`/`min-tolerance` aren't exposed as VST3 parameters - see "Not exposed here" below); green otherwise.
- **COM (Dialogue) momentary loudness**: fills bottom-up like IS's cyan portion, orange, via `AutomixProcessor::dialogue_momentary_lufs()` (never gained, so no pre/post distinction on this side) - plus a `{lufs:.1} LUFS` label (or "-inf" at/below `Ebur128Meter::NEGATIVE_INFINITY_DB`).
- **Scale**: a vertical axis (tick marks + numbers) drawn immediately to the right of the IS bar and the COM bar (Ratio doesn't get one, since it shares the loudness bars' scale visually by spanning between them). Its allocated rect is sized to actually contain the drawn tick text, not just the tick line, so egui's layout system - which has no idea what a `Painter` draws outside the rect it was told about - correctly reserves space for it and neighboring columns don't crowd it. The topmost/bottommost labels are inset slightly so they stay fully visible and centered on their tick rather than clipping at the edge of the drawn area.

Integrated LUFS (`AutomixProcessor::bed_integrated_lufs`/`dialogue_integrated_lufs` from an earlier iteration of this GUI) was dropped as a deliberate simplification, not a gap - momentary is what the automix control loop and this display both care about; `UI.md`'s "IS LUFS-I"/"Comm LUFS-I" wishlist items are considered out of scope unless a concrete need for them comes up later.

**Layout padding**: the meter bank, the title, and the controls section all share a consistent 12px left indent so nothing sits flush against the window's left edge; the window content also has top and bottom breathing room (`ui.add_space` before the title and after the last control row).

**Controls** - every parameter from the "Parameters" section above is exposed directly in the same window, still routed through `ParamSetter` (so host automation/undo/preset-recall works identically to the host's own generic parameter UI):
- `target-ratio`, `max-gain-reduction-db`, `attack-ms`, `hold-ms`, `release-ms`, `divergence` - via `nih_plug_egui::widgets::ParamSlider`.
- `bypass`, `mix-dialogue-to-bed` - via a plain `ui.checkbox` bound through `ParamSetter::begin_set_parameter`/`set_parameter`/`end_set_parameter`, since `nih_plug_egui` has no built-in bool-parameter widget (`ParamSlider` only handles continuous/stepped ranges).

## Building

`nih-plug` doesn't publish to crates.io - it's a git dependency. Bundling uses a workspace `xtask` crate (wrapping `nih_plug_xtask::main()`) plus a `.cargo/config.toml` alias:

```shell
cargo xtask bundle com-is-assist-vst3 --release
```

Produces `target/bundled/Com-IS-Assist.vst3`.

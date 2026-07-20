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

### `mix-dialogue` parameter ("bus insert mode")

A bool parameter selecting what channels 0-5 carry on output:

- **On** (default): channels 0-2 (Left/Right/Center) carry leveled Bed + Dialogue mixed in via the `divergence` pan law below; channels 3-5 (LFE/surrounds) are leveled Bed only, Dialogue never reaches them — matches the GStreamer element's `mix_src`, restricted to L/R/C.
- **Off**: all of channels 0-5 are leveled Bed alone, Dialogue not folded in anywhere — matches `is_leveled_src`, for inserting directly on a Bed/IS channel strip where Dialogue is summed in separately downstream.

Channel 6 (Dialogue passthrough) is unaffected by this toggle either way.

### `divergence` parameter ("Voice divergence", `UI.md`)

0-100%, only has an effect while `mix-dialogue` is on. Controls how Dialogue is placed across Left/Center/Right when mixed in, via `com_is_assist_core::automix::mix_dialogue_into_bed`'s equal-power (constant-power) pan law:

- **0%**: Dialogue is Center-only.
- **100%**: Dialogue is split across Left/Right only, each at `1/sqrt(2)` (~-3dB) so the total acoustic power matches Center-only rather than growing louder as it spreads — the standard constant-power crossfade, not a linear one.

## Parameters

- `target-ratio` — LU
- `max-gain-reduction-db` — dB
- `attack-ms` ("Fade down time"), `hold-ms`, `release-ms` ("Recovery time") — milliseconds, range 1-5000ms, 1ms step (changed from the GStreamer element's seconds-based equivalents specifically per this wrapper's UI - see `Specs/TechnicalConcept.md` section 8)
- `automix-enable` — bypass toggle
- `mix-dialogue`, `divergence` — see above

**Not exposed here, unlike the GStreamer element**: `max-tolerance`/`min-tolerance` (fixed at `AutomixEngineConfig::default()`'s values, not user-adjustable in this wrapper - a deliberate simplification of the VST3 parameter surface). VAD-related parameters (`UI.md`'s "voice detector comm/IS") are M3 scope, not yet implemented in either wrapper.

## Meter display (minimal custom GUI)

Resolved: `nih-plug`'s parameter setter (`ParamMut`) is deliberately `pub(crate)` - a plugin cannot programmatically update its own parameter's displayed value from `process()`, so live meters can only be shown via a custom GUI reading shared atomic state directly. This is a deliberate, scoped exception to the "no custom GUI, parameters only" non-goal (§11 of `TechnicalConcept.md`) - the GUI exists *only* to display live meters, not to duplicate parameter controls (those stay host-native).

Implemented via `nih_plug_egui` (`create_egui_editor`), matching `nih-plug`'s own gain-with-GUI example pattern: an `Arc<AtomicF32>` field on the plugin struct is written from `process()` (only while the editor is actually open, to keep the audio thread's hot path clear the rest of the time) and read from the `editor()` closure each GUI frame.

- **Step 1 (done)**: gain reduction, shown as a dB label + a progress bar normalized against a 24dB reference range.
- **Step 2 (planned)**: add a second `Arc<AtomicF32>` for the COM/IS loudness ratio (`AutomixProcessor::applied_ratio_lu()`, already available) the same way, alongside the gain-reduction bar.

## Building

`nih-plug` doesn't publish to crates.io - it's a git dependency. Bundling uses a workspace `xtask` crate (wrapping `nih_plug_xtask::main()`) plus a `.cargo/config.toml` alias:

```shell
cargo xtask bundle com-is-assist-vst3 --release
```

Produces `target/bundled/Com-IS-Assist.vst3`.

# Com-IS-Assist

[![Build Linux](https://github.com/andyweiss/Com-IS-Assist/actions/workflows/build-linux.yml/badge.svg)](https://github.com/andyweiss/Com-IS-Assist/actions/workflows/build-linux.yml)

A realtime broadcast mix assist/ automixer that ducks an International Sound "Bed" (2-6 channels) against a Dialogue "Com" (mono commentary) channel, based on a measured COM/IS loudness ratio. The concept follows Jonas Engel's bachelor thesis at Hochschule Darmstadt, and the reference implementation it grew out of is included under `Specs/Ressouces/Com-IS Plugin/` (a Reaper JSFX loudness meter, LGPL-licensed, Copyright Cockos Incorporated / Jonas Engel).

Ships as two thin wrappers around one shared Rust DSP core:
- A **GStreamer element** (`comisassist`) — the primary broadcast deployment target. Bed is 2-6ch, dynamically negotiated via caps.
- A **VST3 plugin** (`Com-IS-Assist`) — for DAW-based testing and validation against the original JSFX meter in Reaper. Fixed 8-channel bus (6ch Bed + mono Dialogue + 1 unused), with a mix/duck-only toggle and a live meter GUI (gain reduction, IS/COM loudness, ratio, both voice LEDs, current state and per-stage loop contributions).

## How it works

Two voice-activity detectors — one on the Dialogue (COM), one on the Bed (IS) — select one of four states, and that state alone decides what the mixer does:

| Voice on COM | Voice on IS | What happens |
|---|---|---|
| no | no | Bed recovers to unity |
| yes | no | Bed is ducked until the **target ratio** is met |
| no | yes | Bed recovers to unity *faster* (something on the Bed is worth hearing) |
| yes | yes | Bed is ducked until the higher **over-voice ratio** is met |

How much to duck is computed **feed-forward** from the dry signals. Because reducing the Bed by 1 dB raises the COM/IS ratio by exactly 1 LU, the required reduction is simply `target − (COM − BED)` — no feedback loop, so no windup and no pumping.

That reduction is applied by three cascaded gain stages whose detectors see the Bed at different speeds (3 s short-term, 400 ms momentary, and a ~50 ms K-weighted detector run at audio-block rate), their reductions adding in dB. Attack and release are not configured but **emerge** from how the stages interact: long and gentle when the programme is steady, very fast when the Bed surges. A single **Speed** control scales them together.

Dialogue is never gained or delayed — only measured and, optionally, mixed back into the Bed's Left/Center/Right channels via a "voice divergence" equal-power pan law (Center-only at 0% up to split-L/R at 100%). Neither wrapper adds any latency.

## Repository layout

```
crates/
  com-is-assist-core/        # shared DSP: loudness metering (ebur128), voice activity (Silero),
                             #   the 4-state machine and the multi-loop gain cascade
  com-is-assist-gstreamer/   # GStreamer element (cdylib, built via cargo-c)
  com-is-assist-vst3/        # VST3 plugin (nih-plug)
  com-is-assist-offline/     # CLI tool: WAV(bed) + WAV(dialogue) -> CSV metrics + rendered WAVs
xtask/                       # VST3 bundler (nih-plug's `cargo xtask bundle`)
Specs/                       # technical concept, design docs, and the original JSFX reference
docs/                        # licensing notes
```

See `Specs/TechnicalConcept.md` for the full technical design and rationale.

## Building

Requires a stable Rust toolchain (`rustup.rs`).

**Run the test suite and the offline CLI tool:**
```shell
cargo test --workspace
cargo run -p com-is-assist-offline -- bed.wav dialogue.wav [output_dir]
```

**Build the GStreamer plugin** (requires GStreamer development packages — `brew install gstreamer gst-plugins-base gst-plugins-good` on macOS, or the equivalent `apt` packages on Linux — plus `cargo install cargo-c`):
```shell
cd crates/com-is-assist-gstreamer
cargo cbuild --release
```
Point `GST_PLUGIN_PATH` at the resulting build directory and load it with `gst-inspect-1.0 comisassist` or in a `gst-launch-1.0`/Strom pipeline.

**Build and bundle the VST3 plugin:**
```shell
cargo xtask bundle com-is-assist-vst3 --release
```
Produces `target/bundled/Com-IS-Assist.vst3`.

## License

GPLv3 — see `LICENSE`. Driven specifically by the VST3 build target (`nih-plug`'s VST3 export depends on the GPLv3-licensed `vst3-sys` crate); see `docs/LICENSING.md` for the full rationale and a breakdown of the (mostly permissive) licenses of the rest of the dependency stack.

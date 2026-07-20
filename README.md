# Com-IS-Assist

A realtime broadcast automixer that ducks an International Sound "Bed" (2-6 channels) against a Dialogue "Com" (mono commentary) channel, based on a measured COM/IS loudness ratio. The concept follows Jonas Engel's bachelor thesis at Hochschule Darmstadt, and the reference implementation it grew out of is included under `Specs/Ressouces/Com-IS Plugin/` (a Reaper JSFX loudness meter, LGPL-licensed, Copyright Cockos Incorporated / Jonas Engel).

Ships as two thin wrappers around one shared Rust DSP core:
- A **GStreamer element** (`comisassist`) — the primary broadcast/OB-van deployment target.
- A **VST3 plugin** (`Com-IS-Assist`) — for DAW-based testing and validation against the original JSFX meter in Reaper.

## How it works

Each ~100ms tick, the plugin measures the short-term BS.1770 loudness of both the Bed and the Dialogue signal, computes their ratio, and — if the ratio drifts outside a configurable tolerance band — smoothly reduces Bed gain (fast attack, hold, slow recovery) until the target ratio is restored. The Bed loudness meter observes the *already-gained* signal (a closed control loop), matching how the ratio behaves on-air. Dialogue is never gained — only measured and, optionally, mixed back in.

## Repository layout

```
crates/
  com-is-assist-core/        # shared DSP: loudness metering (ebur128), ratio engine, gain computer
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

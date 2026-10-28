# Com-IS-Assist

![UI](https://github.com/andyweiss/Com-IS-Assist/blob/main/docs/PluginUI.png "UI")

A realtime broadcast mix assist/ automixer that ducks an International Sound "Bed" (2.0, 5.1, 5.1.4 or 7.1.2) against a Dialogue "Com" (mono commentary) channel, based on a measured COM/IS loudness ratio. The concept follows Jonas Engel's bachelor thesis at Hochschule Darmstadt, and the reference implementation it grew out of is included under `Specs/Ressouces/Com-IS Plugin/` (a Reaper JSFX loudness meter, LGPL-licensed, Copyright Cockos Incorporated / Jonas Engel).

Ships as a **VST3 plugin** (`Com-IS-Assist`) wrapping a shared Rust DSP core. One binary, four plugins - **2.0, 5.1, 5.1.4 and 7.1.2** - each taking the Bed on its main bus and the mono commentary on a sidechain input, with a mix/duck-only toggle and a live meter GUI (gain reduction, IS/COM loudness, ratio, both voice LEDs and current state), plus per-channel-group duck depth controls on the surround formats.

## How it works

Two voice-activity detectors — one on the Dialogue (COM), one on the Bed (IS) — select one of four states, and that state alone decides what the mixer does:

| Voice on COM | Voice on IS | What happens |
|---|---|---|
| no | no | Bed recovers to unity |
| yes | no | Bed is ducked until the **IS voice ratio** is met |
| no | yes | Bed recovers to unity *faster* (something on the Bed is worth hearing) |
| yes | yes | Bed is ducked until the higher **over-voice ratio** is met |

How much to duck is computed **feed-forward** from the dry signals. Because reducing the Bed by 1 dB raises the COM/IS ratio by exactly 1 LU, the required reduction is simply `target − (COM − BED)` — no feedback loop, so no windup and no pumping.

That reduction is applied by three cascaded gain stages whose detectors see the Bed at different speeds (3 s short-term, 400 ms momentary, and a ~50 ms K-weighted detector run at audio-block rate), their reductions adding in dB. Attack and release are not configured but **emerge** from how the stages interact: long and gentle when the programme is steady, very fast when the Bed surges. **Attack** and **Release** set the two directions separately, each scaling all three stages together. **Tolerance** keeps the two quicker stages from chasing small fluctuations, so a mix that needs only a little ducking stays calm. The fast stage can be switched off to compare the result without it.

How much of that reduction each part of the Bed receives is set per channel group (**duck depth**, a ratio from 1:1 down to 1:0 for Front L/R, Center, LFE and Surround L/R, plus Top Front L/R and Top Rear L/R on 5.1.4, and Side L/R and Top Side L/R on 7.1.2), so the surrounds and LFE can be held back while the front channels duck fully. Everything at 1:1 - the default - ducks the whole Bed uniformly.

Dialogue is never gained or delayed — only measured and, optionally, mixed back into the Bed's Left/Center/Right channels via a "voice divergence" equal-power pan law (Center-only at 0% up to split-L/R at 100%; on 2.0, equally into L and R). Neither wrapper adds any latency.

## Using the plugin

The `.vst3` bundle contains four plugins. Pick the one that matches the Bed's format:

| Plugin | Bed (main input and output) | Commentary |
|---|---|---|
| Com-IS-Assist 2.0 | L R | mono sidechain |
| Com-IS-Assist 5.1 | L R C LFE Ls Rs | mono sidechain |
| Com-IS-Assist 5.1.4 | L R C LFE Ls Rs Ltf Rtf Ltr Rtr | mono sidechain |
| Com-IS-Assist 7.1.2 | L R C LFE Ls Rs Sl Sr Tsl Tsr | mono sidechain |

Insert it on the Bed track or bus, and route the commentary to its sidechain input. Each plugin accepts only its own VST3 speaker arrangement, so a track of another format is refused rather than processed with the wrong channel roles. Until the sidechain is connected the commentary reads as silence, and the Bed passes at unity.

The output is the ducked Bed. With **Mix dialogue to bed** on (the default), the commentary is mixed into it as well; switch it off when the commentary is summed separately downstream. The commentary is never passed through on its own channel.

**Install** by copying `Com-IS-Assist.vst3` into the system VST3 folder and rescanning plugins in the DAW:

| OS | VST3 folder |
|---|---|
| macOS | `~/Library/Audio/Plug-Ins/VST3/` (or `/Library/Audio/Plug-Ins/VST3/` for all users) |
| Windows | `C:\Program Files\Common Files\VST3\` |
| Linux | `~/.vst3/` |

Sessions saved with releases up to v1.0.2 (the single 8-channel plugin) reopen on **Com-IS-Assist 5.1**, with their settings kept. The bus layout changed, so check the routing: the commentary now goes to the sidechain instead of input channel 7.

## Repository layout

```
crates/
  com-is-assist-core/        # shared DSP: loudness metering (ebur128), voice activity (Silero),
                             #   the 4-state machine and the multi-loop gain cascade
  com-is-assist-vst3/        # VST3 plugin (nih-plug)
  com-is-assist-offline/     # CLI tool: WAV(bed) + WAV(dialogue) -> CSV metrics + rendered WAVs
vendor/nih_plug/             # nih-plug with exact VST3 speaker arrangements (see PATCHED.md there)
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

**Build and bundle the VST3 plugin:**
```shell
cargo xtask bundle com-is-assist-vst3 --release

# Stamp a release build with its tag (otherwise it reports "dev build"):
# COMIS_RELEASE_VERSION=v1.2.0 cargo xtask bundle com-is-assist-vst3 --release
```
Produces `target/bundled/Com-IS-Assist.vst3`, containing all four plugins.

The offline tool reads a 10-channel Bed as 5.1.4. Set `COMIS_BED_LAYOUT=7.1.2` to read it as 7.1.2 instead.

## Releases

Pushing a `v*` tag (which is what publishing a GitHub release does) runs `.github/workflows/release.yml` — the only workflow in the repo, so nothing runs on an ordinary commit. It: it builds the VST3 for macOS, Windows and Linux, and attaches one zip per platform to the release. The tag is passed to the build as `COMIS_RELEASE_VERSION` and shown on the plugin's help page, so a published build always names its version and any other build says "dev build".

| Artifact | Target |
|---|---|
| `Com-IS-Assist-macos-arm64.zip` | macOS, Apple Silicon only |
| `Com-IS-Assist-windows-x64.zip` | Windows x86-64 |
| `Com-IS-Assist-linux-x64.zip` | Linux x86-64 |

**No Intel macOS build.** `ort` publishes no prebuilt ONNX Runtime for `x86_64-apple-darwin` — its download list covers `aarch64-apple-darwin` and not the Intel equivalent — so the Silero VAD dependency cannot be built for Intel Macs without supplying an ONNX Runtime by hand. A universal binary is therefore not possible as things stand.

## License

**MIT**, except the VST3 wrapper — see `LICENSE`.

`com-is-assist-core`, `com-is-assist-offline` and `xtask` are MIT. `com-is-assist-vst3` alone is **GPL-3.0-or-later** (`crates/com-is-assist-vst3/LICENSE`), because `nih-plug`'s default `vst3` feature pulls the GPLv3-licensed `vst3-sys` crate. Nothing in this project's own code requires that; the other crates pull no VST3 dependency at all, so only a distributed VST3 binary carries GPLv3 obligations. See `docs/LICENSING.md` for the full rationale, the rest of the dependency stack, and what it would take to make the plugin permissive too.

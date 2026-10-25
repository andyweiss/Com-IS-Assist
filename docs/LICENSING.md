# Licensing

This project is open source (see `Specs/TechnicalConcept.md` section 1). This note captures the licensing implications of the dependencies chosen for the Rust implementation, corrected against actual verification rather than assumption.

## The short version

**Per-crate licensing**, because the GPLv3 obligation comes from exactly one dependency and reaches exactly one crate:

| Crate | License | Why |
|---|---|---|
| `com-is-assist-core` | MIT | No GPL dependency anywhere in its tree |
| `com-is-assist-offline` | MIT | Same |
| `xtask` | MIT | Same |
| `com-is-assist-vst3` | **GPL-3.0-or-later** | `nih_plug`'s default `vst3` feature pulls `vst3-sys`, which is GPLv3 |

Root `LICENSE` is the MIT text; `crates/com-is-assist-vst3/LICENSE` carries the GPLv3 text for the one crate that needs it. Every `Cargo.toml` declares its `license` field accordingly.

**Verified, not assumed** (`cargo tree -p <crate> -e normal`): `com-is-assist-core` and `com-is-assist-offline` pull *zero* `vst3*` crates. The GPLv3 reach really is limited to the plugin wrapper, which is what makes the split meaningful rather than cosmetic.

**What this means in practice.** The DSP core - where all the signal processing actually lives - is MIT and can be used, vendored or relicensed freely. Only a distributed **VST3 binary** carries GPLv3 obligations, because that artifact links `vst3-sys`. Building the VST3 for in-house testing changes nothing; distributing it is what triggers the terms.

## Why the VST3 target forces GPLv3

The project was originally planned around JUCE (C++), which is GPLv3 for non-commercial-license users. After migrating to Rust, the plan switched to `nih-plug` for VST3 - but **this does not remove the GPLv3 obligation**:

- `nih-plug` itself is ISC-licensed (permissive). This is also why the patched copy in `vendor/nih_plug` (see `PATCHED.md` there) can be carried in this repository, as long as its `LICENSE` file stays with it. Its CLAP support (`clap-sys`, MIT/Apache-2.0) is an unconditional dependency.
- Its VST3 export path is gated behind nih-plug's `vst3` **feature**, which is on by default and pulls `vst3-sys` - **GPLv3**, because it wraps Steinberg's VST3 SDK under the SDK's older dual-license terms. Upstream is explicit that the feature exists "mostly for GPL-compliance reasons, since even if you don't use the VST3 wrapper you might otherwise still include a couple (unused) symbols from the `vst3-sys` crate."

So any VST3 plugin built with `nih-plug`'s default export must be able to comply with GPLv3, for essentially the same underlying reason the JUCE-based plan was GPLv3 - only the specific binding changed, not the obligation.

**Two things would remove the GPLv3 dependency rather than work around it**, recorded because they bear on why the current split is where the line falls:

- `clap-sys` is MIT/Apache-2.0 and is already in the tree - nih-plug exports CLAP without the `vst3` feature at all. The plugin format is what carries the obligation, not anything this project wrote.
- Steinberg released VST3 SDK 3.8.0 under **MIT**, and a `vst3` crate (dual MIT/Apache-2.0) exists with more complete bindings than `vst3-sys`. `nih-plug` does not use it.

## The rest of the stack (core DSP, offline tool)

Independent of the VST3 question, the non-VST3 parts of the stack are predominantly permissively licensed:

- `ebur128` — MIT
- `hound` — Apache-2.0
- `rubato` — MIT (verified directly against its `Cargo.toml`)
- `ort` (ONNX Runtime bindings, used for `com_is_assist_core::voice_activity::SileroVad`) — dual MIT/Apache-2.0 (verified directly against its `Cargo.toml`). The `download-binaries` feature fetches a prebuilt ONNX Runtime **static** library at build time and statically links it in (verified directly: `otool -L` on the built VST3 binary shows no `libonnxruntime` runtime dependency, only system frameworks) - not a shared library the deployed artifact depends on at runtime. Network access is needed once per unique (target, feature set, ONNX Runtime version) on the *build* machine only; the download is cached afterward, and the compiled artifact itself runs fully offline. ONNX Runtime itself is Microsoft's, MIT-licensed.

This is what made the per-crate split above possible: `com-is-assist-core` and `com-is-assist-offline` depend on nothing VST3-related, so nothing had to be restructured to separate them - only declared.

## Third-party reference material

`Specs/Ressouces/Com-IS Plugin/` contains the original Reaper JSFX loudness meter this project's concept is based on (see `Specs/TechnicalConcept.md` section 2). It carries its own license header directly in the file (`COM_IS_RATIO_Meter_5.1_V7.2.txt`): Copyright (C) 2021 and later Cockos Incorporated, author Cockos+Jonas Engel, licensed under the LGPL (<https://www.gnu.org/licenses/lgpl.html>). It's included here for reference/attribution, not redistributed as part of this project's own licensed code.

`crates/com-is-assist-core/assets/silero_vad.onnx` is Silero VAD's exported ONNX model, vendored directly (via `include_bytes!`, not fetched at build/runtime) from <https://github.com/snakers4/silero-vad> - Copyright (c) 2020-present Silero Team, MIT-licensed (verified directly against that repository's `LICENSE` file). Used as-is, not modified.

## Accuracy of this document

Every license above was verified against the dependency's own `Cargo.toml` or `LICENSE` file, and the crate-level scoping against `cargo tree`, rather than taken from memory or from a summary. A distribution notice should be generated from the tree directly (`cargo-about`, `cargo-license`) rather than from this page, which is a record of the reasoning rather than a manifest.

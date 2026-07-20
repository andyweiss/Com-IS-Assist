# Licensing

This project is open source (see `Specs/TechnicalConcept.md` section 1). This note captures the licensing implications of the dependencies chosen for the Rust implementation, corrected against actual verification rather than assumption.

## The short version

**Overall project license: GPLv3** (or a GPLv3-compatible license), driven specifically by the VST3 target — not by anything else in the stack.

## Why VST3 forces GPLv3

The project was originally planned around JUCE (C++), which is GPLv3 for non-commercial-license users. After migrating to Rust, the plan switched to `nih-plug` for VST3 — but **this does not remove the GPLv3 obligation**:

- `nih-plug` itself is ISC-licensed (permissive).
- Its VST3 export path (`nih_export_vst3!()`) depends on the `vst3-sys` crate, which is **GPLv3**, because it wraps Steinberg's VST3 SDK under the SDK's older dual-license terms.

So any VST3 plugin built with `nih-plug`'s default export must be able to comply with GPLv3, for essentially the same underlying reason the JUCE-based plan was GPLv3 — only the specific binding changed (`vst3-sys` instead of JUCE), not the obligation itself.

## A relevant, evolving detail — not yet a fix

Steinberg released VST3 SDK 3.8.0 under the **MIT** license, a significant change from the older GPLv3/proprietary dual-license terms. A newer Rust crate, `vst3` (dual MIT/Apache-2.0), exists as a more permissively-licensed alternative to `vst3-sys`, with more complete/up-to-date bindings.

`nih-plug` does not use the permissive `vst3` crate by default today — it still uses `vst3-sys`. If `nih-plug` migrates to the newer crate upstream, or if this project ever writes/uses a different VST3 binding layer built on `vst3`, the VST3 target could plausibly become permissively licensable too. **This is a future revisit, not resolved in the Rust migration** — don't assume it's already fixed.

## The rest of the stack (GStreamer, core DSP, offline tool)

Independent of the VST3 question, the non-VST3 parts of the stack are predominantly permissively licensed:

- `ebur128` — MIT
- `gstreamer-rs`/`gst-plugins-rs` — dual MIT/Apache-2.0
- `hound` — Apache-2.0
- `rubato`, `nnnoiseless` — permissive (verify exact license file before quoting precisely in any distribution notice)

If there's ever a reason to split licensing (e.g. distribute the GStreamer element and core library under a permissive license while keeping only the VST3 crate under GPLv3), that's architecturally possible since `com-is-assist-gstreamer`/`com-is-assist-offline` don't depend on anything VST3-related. Not being pursued now — keeping one project-wide GPLv3 license is simpler and was already the plan's default before this migration.

## Third-party reference material

`Specs/Ressouces/Com-IS Plugin/` contains the original Reaper JSFX loudness meter this project's concept is based on (see `Specs/TechnicalConcept.md` section 2). It carries its own license header directly in the file (`COM_IS_RATIO_Meter_5.1_V7.2.txt`): Copyright (C) 2021 and later Cockos Incorporated, author Cockos+Jonas Engel, licensed under the LGPL (<https://www.gnu.org/licenses/lgpl.html>). It's included here for reference/attribution, not redistributed as part of this project's own licensed code.

## What to do when actually distributing

Before any public release, re-verify every dependency's license file directly (not this document's summary) and generate a proper `LICENSE`/`NOTICE`/third-party-licenses file, likely via `cargo-about` or `cargo-license`, rather than hand-maintaining the list here.

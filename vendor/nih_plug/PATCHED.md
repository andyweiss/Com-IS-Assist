# Vendored, patched `nih_plug`

A copy of the `nih_plug` crate (and its `nih_plug_derive` proc macro) from
https://github.com/robbert-vdh/nih-plug at commit `f36931f7af4646065488a9845d8f8c2f95252c23`. It is
wired in through `[patch]` in the workspace `Cargo.toml`. ISC-licensed; see `LICENSE`.

## Why

The plugin ships as four VST3s with fixed bed formats: 2.0, 5.1, 5.1.4 and 7.1.2. Upstream's VST3
wrapper gets three things wrong for that:

1. **It can't express same-width layouts.** It derives a bus's speaker arrangement from the channel
   count alone. 5.1.4 and 7.1.2 are both ten channels, and upstream has no 10-channel entry at all;
   it reports a generic mask (`L R C LFE Ls Rs Lc Rc Cs Sl`).
2. **It accepts by channel count only.** `set_bus_arrangements` accepts any arrangement with the
   right number of channels. For example, a 7.1.2 track would be accepted by the 5.1.4 plugin and
   processed with the wrong channel roles.
3. **It checks sidechains against the wrong bus.** In `set_bus_arrangements`, the aux bus start
   index was inverted (`if has_main { 0 } else { 1 }`). With a main bus present, the first
   sidechain was checked against the main bus's arrangement, so every main + sidechain request from
   the host was refused. `get_bus_arrangement` already used the right index.

## What changed

Search for `COM-IS PATCH`:

- `src/plugin/vst3.rs`: new `Vst3Plugin::VST3_MAIN_BUS_ARRANGEMENT: Option<u64>`, defaulting to
  `None`. A plugin sets it to the exact VST3 arrangement of its main input and output buses, for
  example `k51_4` or `k71_2`.
- `src/wrapper/vst3/wrapper.rs`:
  - `bus_arrangement` returns that declared arrangement for main buses. For every other bus, and for
    plugins that don't declare one, it falls back to upstream's count table
    (`defined_speaker_arrangement`).
  - `get_bus_arrangement` reports from `bus_arrangement`.
  - `set_bus_arrangements` accepts a request only if it equals `bus_arrangement` for each bus. Counts
    with no defined arrangement keep upstream's count-only check. The aux bus index is fixed.

For this plugin, the result is: main bus `kStereo` / `k51` / `k51_4` / `k71_2`, and sidechain
`kMono`.

The `Cargo.toml` also differs from upstream: its `[workspace]` and `[profile.*]` sections were
removed, since the crate is no longer a workspace root.

## Updating

Copy `src/`, `nih_plug_derive/` and `Cargo.toml` from the new upstream commit, remove `[workspace]`
and `[profile.*]` again, and re-apply the `COM-IS PATCH` hunks. If upstream ever gains proper
speaker-arrangement support, delete this directory and the `[patch]` entry.

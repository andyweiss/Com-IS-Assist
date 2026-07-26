# UI

## Implementation status

Parameters (both wrappers unless noted; see `Specs/GSTdefinitions.md`/`Specs/vstDefinitions.md` for exact names):
- **Implemented**: Com/IS distance (`target-ratio`), Fade down time (`attack-*`), Recovery time (`release-*`), Voice divergence (`divergence`, both wrappers — restricted to Left/Right/Center, see `mix_dialogue_into_bed`; GStreamer's Bed is 2-6ch dynamically negotiated, and has no audible effect below a 3ch/Center-having layout).
- **"voice detector comm" - implemented, but not as an on/off toggle.** Real voice-activity detection (Silero VAD, via `com_is_assist_core::voice_activity::SileroVad`) now gates the ratio/automix control loop directly, replacing the old flat -70dB LUFS-floor "is COM currently silent?" stand-in - see `Specs/TechnicalConcept.md` section 6. Always on, not exposed as a disable-able parameter: turning it off would just reintroduce the exact problem it fixes (Bed getting reduced in response to COM having *some* sound - room tone, static, another open mic - that isn't actually speech).
- **Not yet implemented** (M3 scope): "voice detector IS" (the Bed-side VAD + interview-passthrough override described below - a separate feature from the COM-side gate above), Reset loudness.

Display:
- **Implemented (VST3 only, custom GUI — see `Specs/vstDefinitions.md`'s "GUI (custom, meters + controls)" section)**: current Gain reduction in dB, shown as a red segment carved out of the top of the IS bar (not a separate meter) down to the effective/audible Bed level; IS LUFS momentary (pre-gain) and Comm LUFS momentary, each shown as a vertical bar filling bottom-up (cyan/orange respectively); Ratio in LU, computed against Bed's effective (post-reduction) level so it reflects what's actually audible; Voice activity Comm (green LED, both wrappers - VST3 as a GUI indicator, GStreamer as a read-only `voice-active` property).
- **Deliberately not implemented**: IS/Comm LUFS-I (integrated) — tried, then dropped as an unneeded simplification (momentary is what both the control loop and this display care about; can be revisited if a concrete need comes up).
- **Not yet implemented**: Voice activity IS LED (blocked on the Bed-side VAD/interview-passthrough work above, which hasn't been built).

## PARAMETERS

# Com IS distance:
0 -12 dB defines how many LUFS the com has to be on top of the IS

# Fade down time
 defines the desired time to reach the set distance

# Recovery time
value from slow to fast

# voice detector comm
on/off enables the voice detector in the side chain

# voice detector IS
on/off is set to on and voice is detected and no voice is detected on hei comm track the volume is raised quite fast to unity gain. (this is useful to pass thru interviews)

# Voice divergence
0% = Center only up to 100% = LR only

# Reset loudness
reset the integrated LUFS

## Display

- IS LUFS- for the IS ratio
- Comm LUFS- for the IS ratio
- Ratio in LUFS between top of IS and top of Comm (see reaper 
implementation)
- IS LUFS-I Integrated
- Com LUFS-I

- Voice activity Comm (green LED)
- Voice activity IS (green LED)
- current Gain reduction in db (shown as a red bar) 
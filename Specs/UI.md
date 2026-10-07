# UI

## Implementation status

Parameters (both wrappers unless noted; see `Specs/GSTdefinitions.md`/`Specs/vstDefinitions.md` for exact names):
- **Implemented**: Com/IS distance (`target-ratio`), Over-voice ratio (`overvoice-ratio`), Speed (`speed`), Lookahead (`lookahead-ms`), Interview passthrough on/off (`interview-passthrough*`), Voice divergence (`divergence`, both wrappers — restricted to Left/Right/Center, see `mix_dialogue_into_bed`; GStreamer's Bed is 2-6ch dynamically negotiated, and has no audible effect below a 3ch/Center-having layout).
- **"voice detector comm" - implemented, but not as an on/off toggle.** Real voice-activity detection (Silero VAD, via `com_is_assist_core::voice_activity::SileroVad`) gates the ratio/automix control loop directly, replacing the old flat -70dB LUFS-floor "is COM currently silent?" stand-in - see `Specs/TechnicalConcept.md` section 6. Always on, not exposed as a disable-able parameter: turning it off would just reintroduce the exact problem it fixes (Bed getting reduced in response to COM having *some* sound - room tone, static, another open mic - that isn't actually speech).
- **"voice detector IS" - implemented.** A second, independent `SileroVad` instance runs on a mono downmix of the dry Bed. Like the COM detector it is always on rather than switchable; what *is* switchable is the behavior it unlocks — see "Interview passthrough" below. Together the two detectors select the automix state (section 5.1 of `TechnicalConcept.md`).
- **Not yet implemented**: Reset loudness.

Display:
- **Implemented (VST3 only, custom GUI — see `Specs/vstDefinitions.md`'s "GUI (custom, meters + controls)" section)**: a tall, professional-bargraph-style meter bank (left side of the window, parameters on the right) with current Gain reduction in dB as its own standalone vertical bar (top-down fill, own 0-48dB scale) next to IS LUFS momentary (pre-gain) and Comm LUFS momentary, each shown as a vertical bar filling bottom-up (cyan/orange respectively); Ratio in LU, computed against Bed's effective (post-reduction) level so it reflects what's actually audible; Voice activity Comm **and** Voice activity IS (green LEDs, under their respective bars); a live **state readout** naming which of the four automix states is currently active, and a **loop readout** showing what each of the three control loops is contributing in dB. GStreamer exposes the equivalents as read-only `voice-active`/`voice-active-is`/`current-mix-state` properties.
- **Deliberately not implemented**: IS/Comm LUFS-I (integrated) — tried, then dropped as an unneeded simplification (momentary is what both the control loop and this display care about; can be revisited if a concrete need comes up).

## Behavior: four states

The mixer does exactly one of four things at any moment, chosen by the two voice detectors. This is the whole behavioral contract — see `Specs/TechnicalConcept.md` section 5.1.

| Voice on Comm | Voice on IS | What happens |
|---|---|---|
| no | no | Bed recovers to unity |
| yes | no | Bed is ducked until **Com/IS distance** is met |
| no | yes | Bed recovers to unity *faster* — only if *Interview passthrough* is on, otherwise treated as the first row |
| yes | yes | Bed is ducked until the higher **Over-voice ratio** is met |

How *fast* each of those happens is set by **Speed**, not by per-state times.

## PARAMETERS

# Com IS distance:
0 -12 dB defines how many LUFS the com has to be on top of the IS

# Speed
How quickly the mixer responds, as a single control. There are no separate attack/release times any more: the leveling is built from three control loops at different speeds (a slow one that sets the overall balance, a faster one that follows the bed, and a very fast one that catches sudden bed surges), and the resulting attack and release behaviour emerges from how they interact. Long and gentle when the programme is steady, very quick when something jumps. Speed scales all three together: raise it if the mix feels sluggish, lower it if it breathes.

# Lookahead
0 by default. Delays the audio slightly so the control loops can see a transient coming and act before it reaches the output. Costs exactly that much latency, which the plugin reports to the host, so leave it at 0 for live/OB use unless you specifically want it.

# Over-voice ratio
The distance required when Comm *and* IS both have voice (a possible over-voice situation, e.g. commentary over an interview or a speaking crowd mic). An absolute value, not an offset on Com/IS distance, and normally set higher than it so the commentator stays intelligible over the competing voice.

# Interview passthrough
on/off. Enables the behavior above. **Off by default**: a loud PA or stadium announcement also registers as voice on IS, and bringing the Bed back up fast on that is risky on air, so it has to be switched on deliberately.

# voice detector comm
Always on (not a parameter) — see Implementation status.

# voice detector IS
Always on (not a parameter) — see Implementation status. The switchable part is *Interview passthrough*.

# Voice divergence
0% = Center only up to 100% = LR only


## Display

- IS LUFS- for the IS ratio
- Comm LUFS- for the IS ratio
- Ratio in LUFS between top of IS and top of Comm (see reaper
implementation)

- Voice activity Comm (green LED)
- Voice activity IS (green LED)
- Current automix state (text)
- current Gain reduction in db (shown as a red bar)

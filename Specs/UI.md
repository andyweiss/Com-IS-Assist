# UI

## Implementation status

Parameters (both wrappers unless noted; see `Specs/GSTdefinitions.md`/`Specs/vstDefinitions.md` for exact names):
- **Implemented**: IS voice ratio (`target-ratio`, formerly labelled "Com/IS distance"), Voice detect gate (`com-gate-lufs`), Over-voice ratio (`overvoice-ratio`), Speed (`speed`), Voice divergence (`divergence`, both wrappers — restricted to Left/Right/Center, see `mix_dialogue_into_bed`; GStreamer's Bed is 2-6ch dynamically negotiated, and has no audible effect below a 3ch/Center-having layout).
- **"voice detector comm" - implemented, but not as an on/off toggle.** Real voice-activity detection (Silero VAD, via `com_is_assist_core::voice_activity::SileroVad`) gates the ratio/automix control loop directly, replacing the old flat -70dB LUFS-floor "is COM currently silent?" stand-in - see `Specs/TechnicalConcept.md` section 6. Always on, not exposed as a disable-able parameter: turning it off would just reintroduce the exact problem it fixes (Bed getting reduced in response to COM having *some* sound - room tone, static, another open mic - that isn't actually speech).
- **"voice detector IS" - implemented.** A second, independent `SileroVad` instance runs on a mono downmix of the dry Bed. Like the COM detector it is always on rather than switchable. When it finds voice on the original while Comm is silent, the bed recovers to unity faster than normal; when both carry voice, the higher Over-voice ratio applies. Together the two detectors select the automix state (section 5.1 of `TechnicalConcept.md`).
- **Not yet implemented**: Reset loudness.

Display:
- **Implemented (VST3 only, custom GUI — see `Specs/vstDefinitions.md`'s "GUI (custom, meters + controls)" section)**: a tall, professional-bargraph-style meter bank (left side of the window, parameters on the right) with current Gain reduction in dB as its own standalone vertical bar (top-down fill, own 0-36dB scale) next to IS LUFS momentary (pre-gain) and Comm LUFS momentary, each shown as a vertical bar filling bottom-up, both in the same accent colour the parameter sliders use; Ratio in LU, computed against Bed's effective (post-reduction) level so it reflects what's actually audible; Voice activity Comm **and** Voice activity IS (green LEDs, under their respective bars); a live **state readout** naming which of the four automix states is currently active, and a **duck depth** control per bed channel group (this replaced a per-loop debugging readout, which showed what each of the three control loops was contributing in dB). GStreamer exposes the equivalents as read-only `voice-active`/`voice-active-is`/`current-mix-state` properties.
- **Deliberately not implemented**: IS/Comm LUFS-I (integrated) — tried, then dropped as an unneeded simplification (momentary is what both the control loop and this display care about; can be revisited if a concrete need comes up).

## Behavior: four states

The mixer does exactly one of four things at any moment, chosen by the two voice detectors. This is the whole behavioral contract — see `Specs/TechnicalConcept.md` section 5.1.

| Voice on Comm | Voice on IS | What happens |
|---|---|---|
| no | no | Bed recovers to unity |
| yes | no | Bed is ducked until the **IS voice ratio** is met |
| no | yes | Bed recovers to unity *faster* (voice on the original is worth hearing) |
| yes | yes | Bed is ducked until the higher **Over-voice ratio** is met |

How *fast* each of those happens is set by **Speed**, not by per-state times.

## PARAMETERS

# IS voice ratio
0 -12 dB defines how many LUFS the com has to be on top of the IS. (Originally called "Com/IS distance"; the underlying parameter id is still `target-ratio`.)

# Speed
How quickly the mixer responds, as a single control. There are no separate attack/release times: the leveling is built from three control loops at different speeds (a slow one that sets the overall balance, a faster one that follows the bed, and a very fast one that catches sudden bed surges), and the resulting attack and release behaviour emerges from how they interact. Long and gentle when the programme is steady, very quick when something jumps. Speed scales all three together: raise it if the mix feels sluggish, lower it if it breathes. Ducking is always far quicker than recovery - the fast loop bites immediately while the recovery is carried by the slow loop - so this one control moves the whole envelope without losing that fast-down/slow-up character. The pause tolerance that keeps the bed down through sentence gaps is fixed in real time, so recovery stays cushioned even at high Speed. The range runs from 0.25x to 8x with 1x at the centre of the control. Towards the top end the slow loop stops being the limit - its 3-second loudness measurement only changes so fast - so the highest settings mainly sharpen the two faster loops.

# Duck depth (per bed channel group)
How much of the ducking each group of bed channels gets, as a ratio. At 1:1 the group takes the full reduction - if the mixer decides on 6 dB, those channels come down 6 dB. At 1:0 they are left alone entirely, however hard the rest of the bed is being held down. Anything between scales it proportionally, so 1:0.5 of a 6 dB reduction is 3 dB.

There are four: **Front L/R**, **Center**, **LFE** and **Surround L/R**. They are grouped rather than per channel on purpose - ducking Left differently from Right is heard as the image sliding sideways rather than as less ducking, and the same goes for the surrounds.

The amount of ducking is still worked out once, for the bed as a whole; this only decides how it is shared out. Leaving every group at 1:1 behaves exactly as before.

It is there because the bed is not all the same material. The surrounds usually carry atmosphere that was never competing with the commentator, so ducking them as hard as the front just drains the scene; the LFE carries nothing to make intelligible at all. Holding those back while the front channels duck fully keeps the commentary clear without flattening everything around it.

# Over-voice ratio
The distance required when Comm *and* IS both have voice (a possible over-voice situation, e.g. commentary over an interview or a speaking crowd mic). An absolute value, not an offset on the IS voice ratio, and normally set higher than it so the commentator stays intelligible over the competing voice.

# voice detector comm
Always on (not a parameter) — see Implementation status.

# voice detector IS
Always on (not a parameter) — see Implementation status. When it detects voice on the original while Comm is silent, the bed recovers to unity faster than normal. This is always active; there is no switch.

# Voice detect gate
A level below which voice detection on the Comm channel is ignored, in LUFS. The detector hears the programme bed bleeding into the commentary mic and correctly identifies it as speech - so without this the bed ducks against its own crosstalk. Spill is always far quieter than the commentator, so a level threshold separates them cleanly. Set it 20-30 dB below the commentator's normal level; the default of -32 LUFS is already engaged rather than parked at the bottom of the range. The region it switches detection off in is shaded grey on the Comm meter, and the Comm voice lamp shows a grey cross whenever the gate is currently suppressing detection - so you can set it by eye against the signal rather than by number.

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

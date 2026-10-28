# UI

## What is implemented

Parameters (see `Specs/vstDefinitions.md` for exact names):
- **Implemented**: IS voice ratio (`target-ratio`, formerly labelled "Com/IS distance"), Voice detect gate (`com-gate-lufs`), Over-voice ratio (`overvoice-ratio`), Attack (`attack-ms`), Release (`release-s`), Tolerance (`tolerance-lu`), Fast stage (`fast-stage`), Voice divergence (`divergence` — restricted to Left/Right/Center, see `mix_dialogue_into_bed`).
- **"voice detector comm" - implemented, but not as an on/off toggle.** Real voice-activity detection (Silero VAD, via `com_is_assist_core::voice_activity::SileroVad`) gates the ratio/automix control loop directly, replacing the old flat -70dB LUFS-floor "is COM currently silent?" stand-in - see `Specs/TechnicalConcept.md` section 6. Always on, not exposed as a disable-able parameter: turning it off would just reintroduce the exact problem it fixes (Bed getting reduced in response to COM having *some* sound - room tone, static, another open mic - that isn't actually speech).
- **"voice detector IS" - implemented.** A second, independent `SileroVad` instance runs on a mono downmix of the dry Bed. Like the COM detector it is always on rather than switchable. When it finds voice on the original while Comm is silent, the bed recovers to unity faster than normal; when both carry voice, the higher Over-voice ratio applies. Together the two detectors select the automix state (section 5.1 of `TechnicalConcept.md`).

Display:
- **Implemented (VST3 only, custom GUI — see `Specs/vstDefinitions.md`'s "GUI (custom, meters + controls)" section)**: a tall, professional-bargraph-style meter bank (left side of the window, parameters on the right) with current Gain reduction in dB as its own standalone vertical bar (top-down fill, own 0-36dB scale) next to IS LUFS momentary (pre-gain) and Comm LUFS momentary, each shown as a vertical bar filling bottom-up, both in the same accent colour the parameter sliders use; Ratio in LU, computed against Bed's effective (post-reduction) level so it reflects what's actually audible; Voice activity Comm **and** Voice activity IS (green LEDs, under their respective bars); a live **state readout** naming which of the four automix states is currently active, and a **duck depth** control per bed channel group (this replaced a per-loop debugging readout, which showed what each of the three control loops was contributing in dB).
- **Deliberately not implemented**: IS/Comm LUFS-I (integrated) — tried, then dropped as an unneeded simplification (momentary is what both the control loop and this display care about; can be revisited if a concrete need comes up).

## Behavior: four states

The mixer does exactly one of four things at any moment, chosen by the two voice detectors. This is the whole behavioral contract — see `Specs/TechnicalConcept.md` section 5.1.

| Voice on Comm | Voice on IS | What happens |
|---|---|---|
| no | no | Bed recovers to unity |
| yes | no | Bed is ducked until the **IS voice ratio** is met |
| no | yes | Bed recovers to unity *faster* (voice on the original is worth hearing) |
| yes | yes | Bed is ducked until the higher **Over-voice ratio** is met |

How *fast* each of those happens is set by **Attack** and **Release**, not by per-state times.

## PARAMETERS

# IS voice ratio
0 -12 dB defines how many LUFS the com has to be on top of the IS. (Originally called "Com/IS distance"; the underlying parameter id is still `target-ratio`.)

# Help page
The **? Help** button next to Bypass opens an about/help overlay: the running version, who made it and whose idea it was based on, which inputs carry what (the bed on the main input, in that format's channel order, and the commentary on the mono sidechain; the output is the ducked bed, with the commentary mixed in when "Mix dialogue to bed" is on), and a one-line description of every control. It opens over the mixer rather than replacing it, so the meters stay visible and in place while you read.

The version is the GitHub release tag for a released build; a build made locally says "dev build" instead, so it is always clear which one is running.

# Attack and Release
How quickly the mixer ducks (Attack) and how quickly it recovers (Release), set separately.

The leveling is built from three control loops at different speeds: a slow one that sets the overall balance, a faster one that follows the bed, and a very fast one that catches sudden bed surges. **Attack** is the time of the loop you hear ducking, the one that follows the bed: 150 ms by default, 20-600 ms. **Release** is the time of the loop you hear recovering, the slow one that holds most of the reduction: 3 s by default, 0.4-12 s. The other loops scale along, so the mixer keeps its character, biting immediately on a surge and settling in behind it, at every setting. The defaults sit in the middle of each control.

The pause tolerance that keeps the bed down through sentence gaps is fixed in real time, so recovery stays cushioned even at a short Release.

# Tolerance
How far the balance may drift before the mixer reacts quickly. 2 LU by default, 0-6 LU.

The two quicker loops are built to grab fast and let go slowly. On a programme whose level moves around, that makes them ride the peaks, so a mix that needs only a little ducking used to get considerably more, and the gain kept moving. Small fluctuations inside the tolerance are now left alone, and only larger ones are acted on (less the tolerance). A genuine surge is still caught. The overall balance is never affected: the slow loop still holds the ratio exactly.

The ratio meter's green "on target" band follows this control (at least 1 LU wide, so it stays readable), so the meter and the mixer agree on what "on target" means. 0 reacts to every fluctuation, which was the behaviour before this control existed.

# Fast stage
Switches the very fast loop on or off. On by default. It reacts within milliseconds to a sudden surge in the bed, such as a crowd roar, before the slower loops have noticed it. That is also what makes the mixer busy: switched off, the gain moves roughly half as much, but a sudden surge takes up to a few hundred milliseconds longer to be caught. It is a switch so the two can be compared on the actual programme.

# Duck depth (per bed channel group)
How much of the ducking each group of bed channels gets, as a ratio. At 1:1 the group takes the full reduction - if the mixer decides on 6 dB, those channels come down 6 dB. At 1:0 they are left alone entirely, however hard the rest of the bed is being held down. Anything between scales it proportionally, so 1:0.5 of a 6 dB reduction is 3 dB.

The 5.1 version has four groups: **Front L/R**, **Center**, **LFE** and **Surround L/R**. The 5.1.4 version adds **Top Front L/R** and **Top Rear L/R** for the height layer. The 7.1.2 version adds **Side L/R** and **Top Side L/R**; on 7.1.2, Surround L/R is the rear pair. The 2.0 version has no duck depth at all: with a single pair there is nothing to distribute the ducking across, so it would only be a weaker duck, which is what the ratios are for. They are grouped rather than per channel on purpose: ducking Left differently from Right is heard as the image sliding sideways rather than as less ducking, and the same goes for the surround, side and height pairs.

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

# Com IS Automix Plugin

## General Concept

This Project Goal is to have a GStreamer Plugin and vst3 Plugin for realtime International sound (Bed) and Com (dialogue) automix based on the concept of the loudness meter com is ratio com/is/ratio jonas engel ba bachelor hda h_da hochschule darmstadt

## Inputs and Outputs

The plugin schould have two inputs:
- Bed (2 to 6Channels ) 
- Dialogue (mono channel)

and three outputs:

- Bed (2 to 6Channels ) -> leveled
- Dialogue (mono channel) 
- Mix: sum of Bed and Dialogue


## Automixer Concept

According to the Work ov Jonas Egel the goal is to measure the R128 Loudness of the Bed and the Dialogue and calculate a Ratio. Based on the Ratio the level of the bed is reduced until the desired ratio is met.

The voice is usually very dirty with IS spill on it hence a voice cleaner is needet on the measurig path in front of the r128 meter section. A AI assisted Voice recocnition tool could also help just to measure the loudness of the dialog when it conains acutal voice.

A scond focus is on the fade in and fade out times of the bed the goal is to have an adaptive and smooth leveling as possible: fast to duck the bed down, then hold, then a slow, smooth recovery back to unity, so the leveling doesn't pump or flutter.


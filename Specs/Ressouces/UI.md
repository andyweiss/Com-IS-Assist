# UI

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
/// Applies a linearly-interpolated, **per-channel** gain across one chunk of interleaved audio,
/// instead of a single flat scalar.
///
/// The gain is recomputed once per audio sub-chunk (see `AutomixProcessor::process_bed`), and
/// multiplying a whole chunk by one constant would produce a hard step in amplitude at every chunk
/// boundary whenever the gain is moving - audible as a "zipper"/clicking artifact. Ramping linearly
/// from `gain_start` (where the previous chunk ended) to `gain_end` removes the discontinuity while
/// still arriving at exactly `gain_end` on the last frame, so the next chunk continues with no seam.
///
/// Per channel rather than one gain for all of them because that is how
/// `AutomixEngineConfig::duck_depth` reaches the audio: the channels can legitimately sit at
/// different gains. `gain_start`/`gain_end` are indexed by position in the interleaved frame and
/// must each cover `channels`; a channel beyond their length is left untouched rather than
/// silenced, so a mis-sized slice can never zero audio.
pub fn apply_ramped_gain_per_channel(samples: &mut [f32], channels: u32, gain_start: &[f32], gain_end: &[f32]) {
    let channels = channels as usize;
    if channels == 0 || samples.is_empty() {
        return;
    }
    let frame_count = samples.len() / channels;
    if frame_count == 0 {
        return;
    }
    let last_frame = (frame_count.saturating_sub(1)) as f32;
    for frame in 0..frame_count {
        // A single-frame chunk has no span to ramp across, so it simply lands on the end value.
        let t = if frame_count > 1 { frame as f32 / last_frame } else { 1.0 };
        for channel in 0..channels {
            let (Some(start), Some(end)) = (gain_start.get(channel), gain_end.get(channel)) else {
                continue;
            };
            samples[frame * channels + channel] *= start + (end - start) * t;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_linearly_from_start_to_end_across_the_block() {
        let mut samples = vec![1.0_f32; 5]; // mono, 5 frames
        apply_ramped_gain_per_channel(&mut samples, 1, &[0.0], &[1.0]);
        assert_eq!(samples, vec![0.0, 0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn ramps_each_channel_independently() {
        let mut samples = vec![1.0_f32; 8]; // stereo, 4 frames
        apply_ramped_gain_per_channel(&mut samples, 2, &[0.0, 1.0], &[1.0, 1.0]);
        assert_eq!(
            samples,
            vec![0.0, 1.0, 1.0 / 3.0, 1.0, 2.0 / 3.0, 1.0, 1.0, 1.0]
        );
    }

    #[test]
    fn flat_gain_when_start_equals_end() {
        let mut samples = vec![2.0_f32; 6];
        apply_ramped_gain_per_channel(&mut samples, 2, &[0.5, 0.5], &[0.5, 0.5]);
        assert_eq!(samples, vec![1.0; 6]);
    }

    #[test]
    fn single_frame_block_uses_the_end_gain() {
        let mut samples = vec![1.0_f32, 1.0_f32];
        apply_ramped_gain_per_channel(&mut samples, 2, &[0.0, 0.0], &[0.5, 0.5]);
        assert_eq!(samples, vec![0.5, 0.5]);
    }

    /// A gain slice shorter than the channel count must leave the uncovered channels alone. The
    /// alternative - treating a missing entry as zero - would silence real audio on a mistake that
    /// is easy to make and hard to hear the cause of.
    #[test]
    fn channels_beyond_the_gain_slice_are_left_untouched() {
        let mut samples = vec![1.0_f32; 6]; // 3 channels, 2 frames
        apply_ramped_gain_per_channel(&mut samples, 3, &[0.0, 0.0], &[0.5, 0.5]);
        assert_eq!(samples, vec![0.0, 0.0, 1.0, 0.5, 0.5, 1.0]);
    }
}

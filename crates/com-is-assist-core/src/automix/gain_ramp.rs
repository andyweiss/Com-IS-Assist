/// Applies a linearly-interpolated gain across one chunk of interleaved audio, instead of a single
/// flat scalar.
///
/// The gain is recomputed once per audio sub-chunk (see `AutomixProcessor::process_bed`), and
/// multiplying a whole chunk by one constant would produce a hard step in amplitude at every chunk
/// boundary whenever the gain is moving - audible as a "zipper"/clicking artifact. Ramping linearly
/// from `gain_start` (where the previous chunk ended) to `gain_end` removes the discontinuity while
/// still arriving at exactly `gain_end` on the last frame, so the next chunk continues with no seam.
pub fn apply_ramped_gain(samples: &mut [f32], channels: u32, gain_start: f32, gain_end: f32) {
    let channels = channels as usize;
    if channels == 0 || samples.is_empty() {
        return;
    }
    let frame_count = samples.len() / channels;
    if frame_count <= 1 {
        for sample in samples.iter_mut() {
            *sample *= gain_end;
        }
        return;
    }
    let last_frame = (frame_count - 1) as f32;
    for frame in 0..frame_count {
        let t = frame as f32 / last_frame;
        let gain = gain_start + (gain_end - gain_start) * t;
        for channel in 0..channels {
            samples[frame * channels + channel] *= gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_linearly_from_start_to_end_across_the_block() {
        let mut samples = vec![1.0_f32; 5]; // mono, 5 frames
        apply_ramped_gain(&mut samples, 1, 0.0, 1.0);
        assert_eq!(samples, vec![0.0, 0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn applies_the_same_interpolated_gain_to_every_channel_in_a_frame() {
        let mut samples = vec![1.0_f32; 8]; // stereo, 4 frames
        apply_ramped_gain(&mut samples, 2, 0.0, 1.0);
        assert_eq!(samples, vec![0.0, 0.0, 1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0, 1.0, 1.0]);
    }

    #[test]
    fn flat_gain_when_start_equals_end() {
        let mut samples = vec![2.0_f32; 6];
        apply_ramped_gain(&mut samples, 2, 0.5, 0.5);
        assert_eq!(samples, vec![1.0; 6]);
    }

    #[test]
    fn single_frame_block_uses_the_end_gain() {
        let mut samples = vec![1.0_f32, 1.0_f32];
        apply_ramped_gain(&mut samples, 2, 0.0, 0.5);
        assert_eq!(samples, vec![0.5, 0.5]);
    }
}

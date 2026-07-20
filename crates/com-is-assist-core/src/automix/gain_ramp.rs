/// Applies a linearly-interpolated gain across one tick's interleaved audio, instead of a single
/// flat scalar for the whole block.
///
/// `AutomixEngine`/`GainComputer` only decide one gain value per ~100ms tick, but multiplying an
/// entire tick's samples by a single constant produces a hard step in amplitude at every tick
/// boundary whenever the gain is changing between ticks (i.e. during essentially any attack or
/// release) - audible as a "zipper"/clicking artifact repeating every tick. Ramping linearly from
/// `gain_start` (the value the previous tick's ramp ended on) to `gain_end` (this tick's newly
/// computed target) removes the discontinuity while still reaching exactly `gain_end` by the last
/// frame, so the next tick can continue the ramp from there with no seam.
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

/// The instantaneous gain at `frame` frames into an overall ramp from `start_gain` (at frame 0)
/// to `end_gain` (at frame `total_frames`). `frame` is clamped to `total_frames`, so querying past
/// the ramp's end just holds at `end_gain` - callers don't need to special-case "ramp already
/// finished."
///
/// Unlike [`apply_ramped_gain`] (which always ramps across exactly the one block it's given),
/// this lets a single logical ramp span many smaller, independently-sized chunks as they arrive
/// (a streaming/low-latency caller doesn't get to choose "the whole tick" as one block) - each
/// chunk just needs to know its own starting offset into the overall ramp.
pub fn ramp_value_at(start_gain: f32, end_gain: f32, frame: u64, total_frames: u64) -> f32 {
    if total_frames == 0 {
        return end_gain;
    }
    let t = frame.min(total_frames) as f32 / total_frames as f32;
    start_gain + (end_gain - start_gain) * t
}

/// Applies part of an overall ramp (see [`ramp_value_at`]) to one chunk of interleaved audio,
/// where `chunk_start_frame` is this chunk's own offset into that ramp. Advancing
/// `chunk_start_frame` by each chunk's frame count across successive calls reproduces the same
/// smooth ramp [`apply_ramped_gain`] would produce for one whole block, without requiring the
/// caller to buffer a full tick before applying any gain at all.
pub fn apply_ramped_gain_at(
    samples: &mut [f32],
    channels: u32,
    start_gain: f32,
    end_gain: f32,
    chunk_start_frame: u64,
    total_frames: u64,
) {
    let channels = channels as usize;
    if channels == 0 || samples.is_empty() {
        return;
    }
    let frame_count = samples.len() / channels;
    for frame in 0..frame_count {
        let gain = ramp_value_at(start_gain, end_gain, chunk_start_frame + frame as u64, total_frames);
        for channel in 0..channels {
            samples[frame * channels + channel] *= gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_value_matches_a_whole_block_ramp_at_the_endpoints() {
        assert_eq!(ramp_value_at(0.0, 1.0, 0, 10), 0.0);
        assert_eq!(ramp_value_at(0.0, 1.0, 10, 10), 1.0);
        assert_eq!(ramp_value_at(0.0, 1.0, 5, 10), 0.5);
    }

    #[test]
    fn ramp_value_holds_at_end_gain_past_the_ramp() {
        assert_eq!(ramp_value_at(0.0, 1.0, 15, 10), 1.0);
    }

    #[test]
    fn ramp_value_with_zero_total_frames_uses_end_gain() {
        assert_eq!(ramp_value_at(0.0, 1.0, 0, 0), 1.0);
    }

    #[test]
    fn chunked_ramp_matches_a_single_whole_block_ramp() {
        let mut whole = vec![1.0_f32; 10]; // mono, 10 frames, one call
        apply_ramped_gain_at(&mut whole, 1, 0.0, 1.0, 0, 10);

        // Same overall ramp, split across three independently-sized chunks arriving separately.
        let mut chunk_a = vec![1.0_f32; 3];
        let mut chunk_b = vec![1.0_f32; 4];
        let mut chunk_c = vec![1.0_f32; 3];
        apply_ramped_gain_at(&mut chunk_a, 1, 0.0, 1.0, 0, 10);
        apply_ramped_gain_at(&mut chunk_b, 1, 0.0, 1.0, 3, 10);
        apply_ramped_gain_at(&mut chunk_c, 1, 0.0, 1.0, 7, 10);
        let chunked: Vec<f32> = chunk_a.into_iter().chain(chunk_b).chain(chunk_c).collect();

        assert_eq!(whole, chunked);
    }

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

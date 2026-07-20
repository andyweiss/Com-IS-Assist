/// The Left/Right/Center channel indices for a Bed layout with the given channel count, matching
/// `Ebur128Meter::bed_channel_map`'s ordering: Left is always index 0 and Right always index 1,
/// but Center only exists for layouts that have one (3, 5, and 6 channels) - stereo (2) and quad
/// (4) don't. `None` for center tells `mix_dialogue_into_bed` to use its no-Center fallback.
pub fn bed_lrc_channels(bed_channels: u32) -> (usize, usize, Option<usize>) {
    match bed_channels {
        3 | 5 | 6 => (0, 1, Some(2)),
        _ => (0, 1, None),
    }
}

/// Mixes dry Dialogue into a Bed buffer, constrained to the Left/Center/Right channels only (never
/// LFE or surrounds), with an equal-power "divergence" crossfade between Center-only and split-LR
/// placement - matches `Specs/Ressouces/UI.md`'s "Voice divergence: 0% = Center only up to 100% =
/// LR only".
///
/// `divergence` is in `[0.0, 1.0]` (0% - 100%). Uses the standard constant-power (quarter-circle
/// sine/cosine) pan law so the total acoustic power contributed by Dialogue stays constant across
/// the whole sweep, rather than dipping or peaking partway through: at `divergence = 1.0`, Left
/// and Right each receive Dialogue at `1/sqrt(2)` (~-3dB), not full amplitude - two channels each
/// at -3dB sum to the same power as one channel at 0dB, matching Center-only at `divergence = 0.0`.
///
/// `center_channel` is `None` for Bed layouts with no dedicated Center channel (stereo, quad - see
/// `bed_lrc_channels`). There's only one physically sensible way to place a mono signal "in the
/// middle" of just Left/Right - send it to both equally - so `divergence` has nothing left to
/// control in that case; the fixed equal-power split is used regardless of its value.
pub fn mix_dialogue_into_bed(
    bed: &mut [f32],
    dialogue: &[f32],
    bed_channels: u32,
    left_channel: usize,
    right_channel: usize,
    center_channel: Option<usize>,
    divergence: f32,
) {
    let bed_channels = bed_channels as usize;
    if bed_channels == 0 {
        return;
    }
    let (center_gain, side_gain) = match center_channel {
        Some(_) => {
            let theta = divergence.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
            (theta.cos(), theta.sin() * std::f32::consts::FRAC_1_SQRT_2)
        }
        None => (0.0, std::f32::consts::FRAC_1_SQRT_2),
    };

    let frame_count = dialogue.len().min(bed.len() / bed_channels);
    for frame in 0..frame_count {
        let sample = dialogue[frame];
        if let Some(center_channel) = center_channel {
            bed[frame * bed_channels + center_channel] += sample * center_gain;
        }
        bed[frame * bed_channels + left_channel] += sample * side_gain;
        bed[frame * bed_channels + right_channel] += sample * side_gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_divergence_is_center_only() {
        let mut bed = vec![0.0_f32; 6]; // 2 frames, 3 channels (L, R, C)
        let dialogue = vec![1.0, 1.0];
        mix_dialogue_into_bed(&mut bed, &dialogue, 3, 0, 1, Some(2), 0.0);
        assert_eq!(bed, vec![0.0, 0.0, 1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn full_divergence_is_split_lr_at_equal_power() {
        let mut bed = vec![0.0_f32; 3]; // 1 frame, 3 channels
        let dialogue = vec![1.0];
        mix_dialogue_into_bed(&mut bed, &dialogue, 3, 0, 1, Some(2), 1.0);
        let expected = std::f32::consts::FRAC_1_SQRT_2;
        assert!((bed[0] - expected).abs() < 1e-6);
        assert!((bed[1] - expected).abs() < 1e-6);
        assert!(bed[2].abs() < 1e-6);
    }

    #[test]
    fn total_power_is_constant_across_the_divergence_sweep() {
        for i in 0..=10 {
            let divergence = i as f32 / 10.0;
            let mut bed = vec![0.0_f32; 3];
            mix_dialogue_into_bed(&mut bed, &[1.0], 3, 0, 1, Some(2), divergence);
            let power: f32 = bed.iter().map(|s| s * s).sum();
            assert!((power - 1.0).abs() < 1e-5, "power should stay ~1.0 at divergence={divergence}, got {power}");
        }
    }

    #[test]
    fn only_touches_left_center_right_never_other_channels() {
        let mut bed = vec![0.5_f32; 6]; // 1 frame, 6 channels, pre-filled with leveled Bed content
        mix_dialogue_into_bed(&mut bed, &[1.0], 6, 0, 1, Some(2), 0.5);
        // Channels 3, 4, 5 (LFE/surrounds) must be untouched.
        assert_eq!(bed[3], 0.5);
        assert_eq!(bed[4], 0.5);
        assert_eq!(bed[5], 0.5);
    }

    #[test]
    fn no_center_channel_falls_back_to_fixed_equal_power_lr_regardless_of_divergence() {
        for divergence in [0.0, 0.3, 0.7, 1.0] {
            let mut bed = vec![0.0_f32; 2]; // 1 frame, 2 channels (stereo, no Center)
            mix_dialogue_into_bed(&mut bed, &[1.0], 2, 0, 1, None, divergence);
            let expected = std::f32::consts::FRAC_1_SQRT_2;
            assert!((bed[0] - expected).abs() < 1e-6, "divergence={divergence}");
            assert!((bed[1] - expected).abs() < 1e-6, "divergence={divergence}");
        }
    }

    #[test]
    fn bed_lrc_channels_has_center_only_for_layouts_that_have_one() {
        assert_eq!(bed_lrc_channels(2), (0, 1, None)); // stereo
        assert_eq!(bed_lrc_channels(3), (0, 1, Some(2)));
        assert_eq!(bed_lrc_channels(4), (0, 1, None)); // quad
        assert_eq!(bed_lrc_channels(5), (0, 1, Some(2)));
        assert_eq!(bed_lrc_channels(6), (0, 1, Some(2)));
    }
}

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
pub fn mix_dialogue_into_bed(
    bed: &mut [f32],
    dialogue: &[f32],
    bed_channels: u32,
    left_channel: usize,
    right_channel: usize,
    center_channel: usize,
    divergence: f32,
) {
    let bed_channels = bed_channels as usize;
    if bed_channels == 0 {
        return;
    }
    let theta = divergence.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    let center_gain = theta.cos();
    let side_gain = theta.sin() * std::f32::consts::FRAC_1_SQRT_2;

    let frame_count = dialogue.len().min(bed.len() / bed_channels);
    for frame in 0..frame_count {
        let sample = dialogue[frame];
        bed[frame * bed_channels + center_channel] += sample * center_gain;
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
        mix_dialogue_into_bed(&mut bed, &dialogue, 3, 0, 1, 2, 0.0);
        assert_eq!(bed, vec![0.0, 0.0, 1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn full_divergence_is_split_lr_at_equal_power() {
        let mut bed = vec![0.0_f32; 3]; // 1 frame, 3 channels
        let dialogue = vec![1.0];
        mix_dialogue_into_bed(&mut bed, &dialogue, 3, 0, 1, 2, 1.0);
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
            mix_dialogue_into_bed(&mut bed, &[1.0], 3, 0, 1, 2, divergence);
            let power: f32 = bed.iter().map(|s| s * s).sum();
            assert!((power - 1.0).abs() < 1e-5, "power should stay ~1.0 at divergence={divergence}, got {power}");
        }
    }

    #[test]
    fn only_touches_left_center_right_never_other_channels() {
        let mut bed = vec![0.5_f32; 6]; // 1 frame, 6 channels, pre-filled with leveled Bed content
        mix_dialogue_into_bed(&mut bed, &[1.0], 6, 0, 1, 2, 0.5);
        // Channels 3, 4, 5 (LFE/surrounds) must be untouched.
        assert_eq!(bed[3], 0.5);
        assert_eq!(bed[4], 0.5);
        assert_eq!(bed[5], 0.5);
    }
}

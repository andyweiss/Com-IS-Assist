//! Per-channel duck depth: one computed reduction, distributed across the Bed's channels.
//!
//! These drive the real `AutomixProcessor` rather than the gain helper directly, because the
//! property worth protecting is end-to-end - that the depth a wrapper sets is the depth the audio
//! receives, in dB, after the ramp and the cascade have had their say.

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor, DuckDepth, MAX_BED_CHANNELS};
use com_is_assist_core::loudness::BedLayout;

const TICK: f64 = 0.1;
const RATE: u32 = 48_000;

fn sine(freq: f64, amp: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| amp * (2.0 * std::f64::consts::PI * freq * i as f64 / RATE as f64).sin() as f32)
        .collect()
}

/// Interleaves one mono signal across all Bed channels, so every channel starts identical and any
/// difference at the output is the doing of `duck_depth` alone.
fn interleave(mono: &[f32], channels: u32) -> Vec<f32> {
    mono.iter().flat_map(|s| std::iter::repeat_n(*s, channels as usize)).collect()
}

fn processor(layout: BedLayout, duck_depth: DuckDepth) -> AutomixProcessor {
    AutomixProcessor::with_layout(
        layout,
        RATE,
        AutomixEngineConfig {
            // Fully open, so the guard band plays no part in what these tests measure.
            com_gate_lufs: -70.0,
            duck_depth,
            ..AutomixEngineConfig::default()
        },
        TICK,
    )
}

/// Runs a loud Bed against a loud COM until the cascade has settled, and returns the dB of
/// reduction each channel ended up receiving, measured against the dry input.
fn settled_reduction_db(duck_depth: DuckDepth) -> Vec<f64> {
    settled_reduction_db_for(BedLayout::Surround51, duck_depth)
}

fn settled_reduction_db_for(layout: BedLayout, duck_depth: DuckDepth) -> Vec<f64> {
    let bed_channels = layout.channel_count();
    let mut p = processor(layout, duck_depth);
    let com = sine(700.0, 0.2, 4_800);
    let dry_mono = sine(400.0, 0.5, 4_800);

    let mut bed = Vec::new();
    for _ in 0..60 {
        p.feed_dialogue(&com).expect("dialogue");
        bed = interleave(&dry_mono, bed_channels);
        p.process_bed(&mut bed).expect("bed");
        p.maybe_run_control_step(true, false);
    }

    // Peak over the back half of the final block, well clear of the ramp into it.
    let channels = bed_channels as usize;
    let frames = bed.len() / channels;
    let mut out = vec![0.0_f64; channels];
    for (channel, slot) in out.iter_mut().enumerate() {
        let wet_peak = (frames / 2..frames)
            .map(|f| bed[f * channels + channel].abs() as f64)
            .fold(0.0_f64, f64::max);
        let dry_peak = (frames / 2..frames).map(|f| dry_mono[f].abs() as f64).fold(0.0_f64, f64::max);
        *slot = -20.0 * (wet_peak / dry_peak).log10();
    }
    out
}

/// The default - every group at 1.0 - must reproduce exactly what the mixer did before the
/// control existed: one reduction, applied uniformly.
#[test]
fn full_depth_on_every_channel_ducks_them_all_equally() {
    let reduction = settled_reduction_db(DuckDepth::default());
    assert!(reduction[0] > 1.0, "expected the Bed to be ducked at all, got {reduction:?}");
    for (channel, db) in reduction.iter().enumerate() {
        assert!(
            (db - reduction[0]).abs() < 0.05,
            "channel {channel} took {db}dB where channel 0 took {}dB",
            reduction[0]
        );
    }
}

/// Depth 0 is the control's whole point at one end: that channel is not ducked at all, however
/// hard the rest of the Bed is being held down.
#[test]
fn zero_depth_leaves_a_channel_untouched() {
    // LFE held out entirely - it carries no intelligibility to protect.
    let reduction = settled_reduction_db(DuckDepth { lfe: 0.0, ..DuckDepth::default() });

    assert!(reduction[0] > 1.0, "the ducked channels should still be ducked, got {reduction:?}");
    assert!(
        reduction[3].abs() < 0.01,
        "LFE should pass at unity, got {}dB of reduction",
        reduction[3]
    );
}

/// And in between it has to be linear **in dB**, which is what makes the number mean something: at
/// 0.5 a channel takes half the decibels, whatever the reduction happens to be.
#[test]
fn partial_depth_scales_the_reduction_in_decibels() {
    let reduction = settled_reduction_db(DuckDepth { surround: 0.5, center: 0.25, ..DuckDepth::default() });

    let full = reduction[0];
    assert!(full > 2.0, "need a decent reduction to halve meaningfully, got {full}dB");
    for surround in [4, 5] {
        assert!(
            (reduction[surround] - full * 0.5).abs() < 0.05,
            "surround channel {surround} took {}dB where half of {full}dB was expected",
            reduction[surround]
        );
    }
    assert!(
        (reduction[2] - full * 0.25).abs() < 0.05,
        "center took {}dB where a quarter of {full}dB was expected",
        reduction[2]
    );
}

/// The group-to-channel mapping has to follow the *channel map*, not fixed stream positions,
/// because which role sits where changes with the channel count: position 2 is Center on a
/// 3/5/6-channel Bed but Left Surround on a 4-channel one. Resolving this in the core is what lets
/// every wrapper expose four honestly-named groups instead of six positional settings that
/// mislabel themselves on every layout but the widest.
#[test]
fn groups_follow_the_channel_map_rather_than_stream_position() {
    let depth = DuckDepth {
        front_lr: 1.0,
        center: 0.8,
        lfe: 0.0,
        surround: 0.4,
        side: 0.7,
        top_front: 0.6,
        top_rear: 0.2,
        top_side: 0.3,
    };

    assert_eq!(&depth.per_channel(2)[..2], &[1.0, 1.0], "2ch is L,R");
    assert_eq!(&depth.per_channel(3)[..3], &[1.0, 1.0, 0.8], "3ch is L,R,C");
    // The case that makes positional settings wrong: no Center, so position 2 is a surround.
    assert_eq!(&depth.per_channel(4)[..4], &[1.0, 1.0, 0.4, 0.4], "4ch is L,R,Ls,Rs");
    assert_eq!(&depth.per_channel(5)[..5], &[1.0, 1.0, 0.8, 0.4, 0.4], "5ch is L,R,C,Ls,Rs");
    assert_eq!(&depth.per_channel(6)[..6], &[1.0, 1.0, 0.8, 0.0, 0.4, 0.4], "6ch is L,R,C,LFE,Ls,Rs");
    assert_eq!(
        depth.per_channel(10),
        [1.0, 1.0, 0.8, 0.0, 0.4, 0.4, 0.6, 0.6, 0.2, 0.2],
        "10ch is L,R,C,LFE,Ls,Rs,Ltf,Rtf,Ltr,Rtr"
    );
    // Same width, different roles - which is why 7.1.2 has to be asked for by layout.
    assert_eq!(
        depth.per_channel_for(BedLayout::Surround712),
        [1.0, 1.0, 0.8, 0.0, 0.4, 0.4, 0.7, 0.7, 0.3, 0.3],
        "7.1.2 is L,R,C,LFE,Ls,Rs,Sl,Sr,Tsl,Tsr"
    );

    // An unsupported count must not silently mute anything.
    assert_eq!(depth.per_channel(7), [1.0; MAX_BED_CHANNELS]);
}

/// Left/Right and the surround pair are deliberately not individually settable - a difference
/// between them is heard as the stereo image sliding sideways rather than as less ducking. The
/// grouping is what guarantees it, so it is worth asserting rather than assuming.
#[test]
fn paired_channels_always_duck_by_the_same_amount() {
    let reduction = settled_reduction_db(DuckDepth { surround: 0.3, ..DuckDepth::default() });

    assert!((reduction[0] - reduction[1]).abs() < 1e-6, "front pair diverged: {reduction:?}");
    assert!((reduction[4] - reduction[5]).abs() < 1e-6, "surround pair diverged: {reduction:?}");
}

/// On 5.1.4 the two height pairs take their own depth, end to end through the processor, and the
/// bed-layer groups behave exactly as they do on 5.1.
#[test]
fn height_pairs_take_their_own_depth_on_5_1_4() {
    let reduction = settled_reduction_db_for(
        BedLayout::Surround514,
        DuckDepth { top_front: 0.5, top_rear: 0.0, ..DuckDepth::default() },
    );

    let full = reduction[0];
    assert!(full > 2.0, "need a decent reduction to compare against, got {full}dB");
    for channel in 0..6 {
        assert!((reduction[channel] - full).abs() < 0.05, "bed channel {channel}: {reduction:?}");
    }
    for top_front in [6, 7] {
        assert!(
            (reduction[top_front] - full * 0.5).abs() < 0.05,
            "top front channel {top_front} took {}dB where half of {full}dB was expected",
            reduction[top_front]
        );
    }
    for top_rear in [8, 9] {
        assert!(reduction[top_rear].abs() < 0.01, "top rear should pass at unity: {reduction:?}");
    }
}

/// The 7.1.2 counterpart: the side and top-side pairs take their own depth, and the 5.1.4 height
/// groups - same channel count, different channels - have no effect at all.
#[test]
fn side_and_top_side_pairs_take_their_own_depth_on_7_1_2() {
    let reduction = settled_reduction_db_for(
        BedLayout::Surround712,
        DuckDepth { side: 0.5, top_side: 0.0, top_front: 0.0, top_rear: 0.0, ..DuckDepth::default() },
    );

    let full = reduction[0];
    assert!(full > 2.0, "need a decent reduction to compare against, got {full}dB");
    for channel in 0..6 {
        assert!((reduction[channel] - full).abs() < 0.05, "channel {channel}: {reduction:?}");
    }
    for side in [6, 7] {
        assert!((reduction[side] - full * 0.5).abs() < 0.05, "side channel {side}: {reduction:?}");
    }
    for top_side in [8, 9] {
        assert!(reduction[top_side].abs() < 0.01, "top side should pass at unity: {reduction:?}");
    }
}

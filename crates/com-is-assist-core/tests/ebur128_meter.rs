use com_is_assist_core::loudness::{ConfigError, Ebur128Meter};
use std::f64::consts::PI;

fn approx_eq(a: f64, b: f64, margin: f64) -> bool {
    (a - b).abs() <= margin
}

fn generate_sine(frequency_hz: f64, amplitude: f64, sample_rate: u32, frame_count: usize) -> Vec<f32> {
    (0..frame_count)
        .map(|i| (amplitude * (2.0 * PI * frequency_hz * i as f64 / sample_rate as f64).sin()) as f32)
        .collect()
}

#[test]
fn reports_the_expected_lufs_for_a_full_scale_sine_tone() {
    const SAMPLE_RATE: u32 = 48000;
    const FREQUENCY_HZ: f64 = 400.0; // clear of the K-weighting shelf/highpass corners

    let mut meter =
        Ebur128Meter::new(&Ebur128Meter::dialogue_channel_map(), SAMPLE_RATE).unwrap();

    // Push 4s of tone in 100ms ticks (matches the project's measurement tick), enough to fill
    // both the 400ms momentary and 3s short-term windows so neither reading is biased by buffer
    // pre-roll.
    let tick_frames = (SAMPLE_RATE / 10) as usize;
    for _ in 0..40 {
        let block = generate_sine(FREQUENCY_HZ, 1.0, SAMPLE_RATE, tick_frames);
        meter.push_frames(&block).unwrap();
    }

    // A 0 dBFS sine measures -0.691 + 10*log10(0.5) ~= -3.70 LUFS once K-weighting is ~flat,
    // which it is at 400 Hz (between the ~38 Hz high-pass and ~1.7 kHz shelf corners of the
    // K-weighting filter).
    assert!(approx_eq(meter.momentary_loudness_db(), -3.7, 0.5));
    assert!(approx_eq(meter.short_term_loudness_db(), -3.7, 0.5));
}

#[test]
fn reports_the_negative_infinity_sentinel_for_silence() {
    const SAMPLE_RATE: u32 = 48000;
    let mut meter =
        Ebur128Meter::new(&Ebur128Meter::dialogue_channel_map(), SAMPLE_RATE).unwrap();

    let silence = vec![0.0f32; (SAMPLE_RATE / 10) as usize];
    for _ in 0..5 {
        meter.push_frames(&silence).unwrap();
    }

    assert_eq!(meter.momentary_loudness_db(), Ebur128Meter::NEGATIVE_INFINITY_DB);
}

#[test]
fn bed_channel_map_rejects_unsupported_channel_counts() {
    assert_eq!(
        Ebur128Meter::bed_channel_map(0),
        Err(ConfigError::UnsupportedChannelCount(0))
    );
    assert_eq!(
        Ebur128Meter::bed_channel_map(7),
        Err(ConfigError::UnsupportedChannelCount(7))
    );
    assert!(Ebur128Meter::bed_channel_map(6).is_ok());
}

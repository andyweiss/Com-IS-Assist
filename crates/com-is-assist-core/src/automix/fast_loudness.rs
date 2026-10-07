use ebur128::Channel;

/// One BS.1770 biquad section (direct form I), used for the two K-weighting stages.
#[derive(Debug, Clone, Copy, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// The two K-weighting sections from BS.1770, derived from the analog prototypes at the actual
/// sample rate rather than hardcoding the published 48kHz coefficients - VST3 hosts and GStreamer
/// pipelines both run at other rates, and using 48kHz coefficients at 44.1kHz would put this
/// detector on a subtly different scale from the `ebur128` loops it has to agree with.
///
/// Same derivation libebur128 itself uses, so the two stay consistent by construction.
fn k_weighting_sections(sample_rate: f64) -> (Biquad, Biquad) {
    // Stage 1: high-shelf ("pre-filter"), modelling the acoustic effect of the head.
    let f0 = 1681.974450955533_f64;
    let gain_db = 3.999843853973347_f64;
    let q = 0.7071752369554196_f64;
    let k = (std::f64::consts::PI * f0 / sample_rate).tan();
    let vh = 10f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b0: (vh + vb * k / q + k * k) / a0,
        b1: 2.0 * (k * k - vh) / a0,
        b2: (vh - vb * k / q + k * k) / a0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / q + k * k) / a0,
        ..Default::default()
    };

    // Stage 2: RLB high-pass.
    let f0 = 38.13547087602444_f64;
    let q = 0.5003270373238773_f64;
    let k = (std::f64::consts::PI * f0 / sample_rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let highpass = Biquad {
        b0: 1.0 / a0,
        b1: -2.0 / a0,
        b2: 1.0 / a0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / q + k * k) / a0,
        ..Default::default()
    };

    (shelf, highpass)
}

/// BS.1770 channel weighting, applied to the **mean square** (energy), not the amplitude - which
/// is why the surround weight is `1.41` (= +1.5dB in power) rather than `10^(1.5/20)`. Copied
/// deliberately from `ebur128`'s own `filter.rs` (`channel_sum *= 1.41`, and the same channel set)
/// rather than from the standard text, because this detector's whole purpose is to agree with that
/// crate's readings to within a fraction of a dB - see the struct doc comment.
fn channel_weight(channel: Channel) -> f64 {
    match channel {
        // LFE and anything else explicitly unused contributes nothing.
        Channel::Unused => 0.0,
        Channel::LeftSurround
        | Channel::RightSurround
        | Channel::Mp060
        | Channel::Mm060
        | Channel::Mp090
        | Channel::Mm090 => 1.41,
        // Counted twice, per the standard.
        Channel::DualMono => 2.0,
        _ => 1.0,
    }
}

/// A short-window, K-weighted loudness detector — the "fast loop" of the multi-loop design
/// (`Specs/TechnicalConcept.md` section 5.2).
///
/// **Why this exists rather than another `Ebur128Meter`:** R128 defines only momentary (400ms) and
/// short-term (3s) windows, and `ebur128`'s mode bits/windows are fixed. The fast stage needs an
/// order of magnitude less integration than momentary so it can respond to transients.
///
/// **Why it is K-weighted rather than a plain RMS:** the fast stage's output is combined with the
/// two R128 loops' by subtraction (it trims their estimate). A plain RMS would sit on a different
/// scale, so the difference would carry a constant offset and the fast stage would pin at its
/// authority limit permanently. Applying the same K-weighting, the same channel weighting and the
/// same `-0.691` offset as BS.1770 puts this reading in LUFS on the identical scale, so the
/// subtraction is meaningful.
///
/// The window is an exponential moving average rather than the rectangular one BS.1770 specifies
/// for its gating blocks. For a trim stage this is the better choice - it is cheaper, needs no
/// history buffer, and produces a continuously-moving value instead of one that steps as samples
/// age out of a rectangular window.
pub struct FastLoudnessMeter {
    channels: usize,
    weights: Vec<f64>,
    /// Two sections per channel, in cascade.
    filters: Vec<(Biquad, Biquad)>,
    /// Per-channel exponential mean square of the K-weighted signal.
    mean_square: Vec<f64>,
    /// EMA coefficient derived from the window length and sample rate.
    coeff: f64,
}

impl FastLoudnessMeter {
    /// `channel_map` uses the same `ebur128::Channel` values as [`crate::loudness::Ebur128Meter`],
    /// so both sides of a ratio can be built from the same map. `window_seconds` is the EMA time
    /// constant (~0.03 for the fast stage).
    pub fn new(channel_map: &[Channel], sample_rate: u32, window_seconds: f64) -> Self {
        let sections = k_weighting_sections(sample_rate as f64);
        let channels = channel_map.len();
        Self {
            channels,
            weights: channel_map.iter().map(|c| channel_weight(*c)).collect(),
            filters: vec![sections; channels],
            mean_square: vec![0.0; channels],
            coeff: (-1.0 / (window_seconds.max(1e-6) * sample_rate as f64)).exp(),
        }
    }

    /// Feeds one chunk of interleaved audio. Any chunk size; call as often as the caller likes -
    /// this is the one detector in the system cheap enough to run at audio-block rate.
    pub fn push_frames(&mut self, interleaved: &[f32]) {
        if self.channels == 0 {
            return;
        }
        for frame in interleaved.chunks_exact(self.channels) {
            for (channel, sample) in frame.iter().enumerate() {
                let sample = *sample as f64;
                // Guard against a non-finite input poisoning the filter state permanently - the
                // biquads are recursive, so one NaN would otherwise stick forever.
                let sample = if sample.is_finite() { sample } else { 0.0 };
                let (shelf, highpass) = &mut self.filters[channel];
                let weighted = highpass.process(shelf.process(sample));
                self.mean_square[channel] =
                    self.mean_square[channel] * self.coeff + weighted * weighted * (1.0 - self.coeff);
            }
        }
    }

    /// Current loudness in LUFS, on the same scale as `Ebur128Meter`'s readings. Returns
    /// [`crate::loudness::Ebur128Meter::NEGATIVE_INFINITY_DB`] for silence, matching that type's
    /// sentinel so consumers need only one silence check.
    pub fn loudness_db(&self) -> f64 {
        let sum: f64 = self
            .mean_square
            .iter()
            .zip(&self.weights)
            .map(|(ms, weight)| weight * ms)
            .sum();
        if sum <= 0.0 {
            return crate::loudness::Ebur128Meter::NEGATIVE_INFINITY_DB;
        }
        let db = -0.691 + 10.0 * sum.log10();
        if db.is_finite() {
            db.max(crate::loudness::Ebur128Meter::NEGATIVE_INFINITY_DB)
        } else {
            crate::loudness::Ebur128Meter::NEGATIVE_INFINITY_DB
        }
    }

    pub fn reset(&mut self) {
        for (shelf, highpass) in &mut self.filters {
            shelf.reset();
            highpass.reset();
        }
        self.mean_square.iter_mut().for_each(|ms| *ms = 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loudness::Ebur128Meter;

    fn sine(frequency_hz: f64, amplitude: f32, sample_rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| amplitude * (2.0 * std::f64::consts::PI * frequency_hz * i as f64 / sample_rate as f64).sin() as f32)
            .collect()
    }

    #[test]
    fn silence_reads_as_the_negative_infinity_sentinel() {
        let mut meter = FastLoudnessMeter::new(&Ebur128Meter::dialogue_channel_map(), 48_000, 0.03);
        meter.push_frames(&vec![0.0_f32; 4800]);
        assert_eq!(meter.loudness_db(), Ebur128Meter::NEGATIVE_INFINITY_DB);
    }

    /// The whole point of K-weighting this detector: it has to agree with `Ebur128Meter` on the
    /// absolute scale, or the fast stage's trim against the R128 loops carries a constant offset.
    /// A steady tone held well past both integration windows should read the same on both.
    #[test]
    fn agrees_with_the_r128_meter_on_a_steady_tone() {
        let sample_rate = 48_000;
        let tone = sine(1_000.0, 0.5, sample_rate, sample_rate as usize * 2);

        let mut fast = FastLoudnessMeter::new(&Ebur128Meter::dialogue_channel_map(), sample_rate, 0.03);
        fast.push_frames(&tone);

        let mut r128 = Ebur128Meter::new(&Ebur128Meter::dialogue_channel_map(), sample_rate).unwrap();
        r128.push_frames(&tone).unwrap();

        let difference = (fast.loudness_db() - r128.momentary_loudness_db()).abs();
        assert!(
            difference < 0.5,
            "fast detector reads {:.2} LUFS, R128 reads {:.2} LUFS - a scale mismatch would make the \
             fast loop's trim meaningless",
            fast.loudness_db(),
            r128.momentary_loudness_db()
        );
    }

    /// It must be genuinely faster than momentary, otherwise it adds nothing over the mid loop.
    #[test]
    fn responds_far_faster_than_the_400ms_momentary_window() {
        let sample_rate = 48_000;
        let burst = sine(1_000.0, 0.5, sample_rate, (sample_rate as f64 * 0.05) as usize);

        let mut fast = FastLoudnessMeter::new(&Ebur128Meter::dialogue_channel_map(), sample_rate, 0.03);
        fast.push_frames(&burst);

        let mut r128 = Ebur128Meter::new(&Ebur128Meter::dialogue_channel_map(), sample_rate).unwrap();
        r128.push_frames(&burst).unwrap();

        assert!(
            fast.loudness_db() > r128.momentary_loudness_db() + 3.0,
            "after only 50ms the fast detector ({:.1}) should have risen well above momentary ({:.1})",
            fast.loudness_db(),
            r128.momentary_loudness_db()
        );
    }

    #[test]
    fn channel_weights_match_the_ebur128_crates_own_table() {
        assert_eq!(channel_weight(Channel::Unused), 0.0, "LFE must contribute nothing");
        assert_eq!(channel_weight(Channel::Left), 1.0);
        assert_eq!(channel_weight(Channel::Center), 1.0);
        // Applied to energy, so +1.5dB is 1.41x, not 10^(1.5/20).
        assert_eq!(channel_weight(Channel::LeftSurround), 1.41);
        assert_eq!(channel_weight(Channel::RightSurround), 1.41);
        assert_eq!(channel_weight(Channel::DualMono), 2.0);
    }
}

/// A one-pole smoother with independent attack and release time constants — the single smoothing
/// primitive the multi-loop gain stages are built from (`Specs/TechnicalConcept.md` section 5.2).
/// Whichever direction `target` moves relative to the current value picks the attack or the release
/// constant, matching standard compressor/expander ballistics.
///
/// Time constants and the time step are passed **per call** rather than stored. That is what lets
/// the three loops share one primitive while running at genuinely different rates: the slow and mid
/// stages step once per ~100ms control tick, the fast stage steps once per audio sub-chunk (a
/// fraction of a millisecond), and the "Speed" macro rescales every constant live without having to
/// rebuild anything or lose the in-flight value.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvelopeDetector {
    current: f64,
}

impl EnvelopeDetector {
    pub fn new(initial: f64) -> Self {
        Self { current: initial }
    }

    pub fn value(&self) -> f64 {
        self.current
    }

    pub fn reset(&mut self, value: f64) {
        self.current = value;
    }

    /// Advances `dt_seconds` toward `target`, returning the new value. A non-positive or
    /// non-finite time constant means "jump straight there", which is the useful degenerate case
    /// rather than a division by zero.
    pub fn process(&mut self, target: f64, attack_seconds: f64, release_seconds: f64, dt_seconds: f64) -> f64 {
        let time_constant = if target > self.current { attack_seconds } else { release_seconds };

        let coeff = if time_constant > 0.0 && time_constant.is_finite() && dt_seconds > 0.0 {
            (-dt_seconds / time_constant).exp()
        } else {
            0.0 // instantaneous
        };

        self.current = target + (self.current - target) * coeff;
        self.current
    }
}

/// Attack/release time constants for [`EnvelopeDetector`], in seconds.
#[derive(Debug, Clone, Copy)]
pub struct EnvelopeDetectorConfig {
    pub attack_seconds: f64,
    pub release_seconds: f64,
}

/// A one-pole exponential smoother with independent attack/release time constants — the basic
/// building block [`crate::automix::GainComputer`]'s Attack-Hold-Release envelope is made of (see
/// `Specs/TechnicalConcept.md` section 5). Whichever direction `target` moves relative to the
/// current value picks the attack or release time constant, matching standard
/// compressor/expander ballistics.
#[derive(Debug, Clone, Copy)]
pub struct EnvelopeDetector {
    config: EnvelopeDetectorConfig,
    /// Ticks (or samples) per second — this detector doesn't care whether it's driven at the
    /// ~10Hz measurement tick rate or full audio sample rate, only the caller does.
    rate_hz: f64,
    current: f64,
}

impl EnvelopeDetector {
    pub fn new(config: EnvelopeDetectorConfig, rate_hz: f64) -> Self {
        Self {
            config,
            rate_hz,
            current: 0.0,
        }
    }

    pub fn value(&self) -> f64 {
        self.current
    }

    pub fn reset(&mut self, value: f64) {
        self.current = value;
    }

    /// Replaces the attack/release time constants without touching `current` - so a config
    /// change (e.g. a live parameter update in a host) takes effect on the *next* `process()`
    /// call without any jump or reset of the envelope's in-progress value.
    pub fn set_config(&mut self, config: EnvelopeDetectorConfig) {
        self.config = config;
    }

    /// Advances the envelope by one step toward `target`, returning the new current value.
    pub fn process(&mut self, target: f64) -> f64 {
        let time_constant = if target > self.current {
            self.config.attack_seconds
        } else {
            self.config.release_seconds
        };

        let coeff = if time_constant <= 0.0 {
            0.0 // instantaneous
        } else {
            (-1.0 / (time_constant * self.rate_hz)).exp()
        };

        self.current = target + (self.current - target) * coeff;
        self.current
    }
}

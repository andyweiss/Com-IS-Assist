use super::envelope_detector::{EnvelopeDetector, EnvelopeDetectorConfig};

/// Configuration for [`GainComputer`]'s Attack-Hold-Release envelope.
///
/// This project deliberately uses a single envelope rather than a multi-stage design: it produces
/// the same fast-duck / slow-recover behavior with far fewer parameters, and matches
/// `Specs/Ressouces/UI.md`'s exposed controls exactly (one "Fade down time," one "Recovery
/// time" — not two attacks/releases each). See `Specs/TechnicalConcept.md` section 5.
#[derive(Debug, Clone, Copy)]
pub struct GainComputerConfig {
    /// UI.md's "Fade down time" — how fast reduction increases.
    pub attack_seconds: f64,
    /// Minimum time to hold the current reduction after an attack event before release is
    /// allowed to begin. This, not a second envelope stage, is what prevents flutter/pumping on
    /// borderline or choppy ratio readings.
    pub hold_seconds: f64,
    /// UI.md's "Recovery time" (slow<->fast) — how fast reduction eases back toward unity.
    pub release_seconds: f64,
    /// Optional additional hard-clamp safety net on the output's rate of change. Not needed
    /// speculatively; add only if listening tests show pumping/zippering the envelope alone
    /// doesn't already prevent.
    pub max_rate_db_per_s: Option<f64>,
}

impl Default for GainComputerConfig {
    fn default() -> Self {
        Self {
            attack_seconds: 0.2,
            hold_seconds: 0.3,
            release_seconds: 3.0,
            max_rate_db_per_s: None,
        }
    }
}

/// Smooths a raw target gain-reduction value (dB, 0 = unity) into an Attack-Hold-Release curve.
pub struct GainComputer {
    envelope: EnvelopeDetector,
    hold_seconds: f64,
    hold_remaining_seconds: f64,
    tick_seconds: f64,
    max_rate_db_per_s: Option<f64>,
    previous_output_db: f64,
}

impl GainComputer {
    pub fn new(config: GainComputerConfig, tick_seconds: f64) -> Self {
        let envelope = EnvelopeDetector::new(
            EnvelopeDetectorConfig {
                attack_seconds: config.attack_seconds,
                release_seconds: config.release_seconds,
            },
            1.0 / tick_seconds,
        );
        Self {
            envelope,
            hold_seconds: config.hold_seconds,
            hold_remaining_seconds: 0.0,
            tick_seconds,
            max_rate_db_per_s: config.max_rate_db_per_s,
            previous_output_db: 0.0,
        }
    }

    pub fn value(&self) -> f64 {
        self.previous_output_db
    }

    pub fn reset(&mut self, value: f64) {
        self.envelope.reset(value);
        self.previous_output_db = value;
        self.hold_remaining_seconds = 0.0;
    }

    /// Replaces the attack/hold/release/rate-limit config without resetting `previous_output_db`,
    /// `hold_remaining_seconds`, or the envelope's in-progress value - so a live parameter change
    /// (e.g. dragging the "Fade down time" slider mid-session) takes effect smoothly on the next
    /// tick rather than causing a jump or restarting any in-progress attack/hold/release.
    /// `tick_seconds` isn't part of `GainComputerConfig` - it's the fixed control-tick rate set at
    /// construction, not something a settings change would ever need to alter.
    pub fn set_config(&mut self, config: GainComputerConfig) {
        self.envelope.set_config(EnvelopeDetectorConfig {
            attack_seconds: config.attack_seconds,
            release_seconds: config.release_seconds,
        });
        self.hold_seconds = config.hold_seconds;
        self.max_rate_db_per_s = config.max_rate_db_per_s;
    }

    /// Advances one tick toward `target_reduction_db` (dB of gain reduction; 0 = unity),
    /// returning this tick's smoothed reduction. Call once per ~100ms tick, same cadence as
    /// `RatioEngine::update`/`AutomixEngine::process_tick`.
    pub fn process_tick(&mut self, target_reduction_db: f64) -> f64 {
        // Keep the hold timer freshly armed while the target is at or above the current output
        // — including while an attack is still converging *and* once it has settled there, so
        // hold only starts truly counting down once the target actually drops below the current
        // output (i.e. release is genuinely being requested). Using `>=` rather than `>` matters
        // here: once the envelope's exponential approach closes to within float precision of the
        // target, `current` becomes bit-identical to `target`, and a strict `>` would stop
        // re-arming the timer — letting it silently count down (and go negative) during a long
        // steady-state hold, so release would start immediately instead of waiting a full
        // `hold_seconds` from when release was actually requested.
        if target_reduction_db >= self.previous_output_db {
            self.hold_remaining_seconds = self.hold_seconds;
        }

        let effective_target = if self.hold_remaining_seconds > 0.0 {
            self.hold_remaining_seconds -= self.tick_seconds;
            // Block release: never ask the envelope for less reduction than is currently
            // applied. If the target is still attacking (higher), it passes through unchanged.
            target_reduction_db.max(self.previous_output_db)
        } else {
            target_reduction_db
        };

        let mut output = self.envelope.process(effective_target);

        if let Some(max_rate) = self.max_rate_db_per_s {
            let max_delta = max_rate * self.tick_seconds;
            let delta = (output - self.previous_output_db).clamp(-max_delta, max_delta);
            output = self.previous_output_db + delta;
            // Keep the envelope's internal state consistent with the clamped output, so the
            // next tick continues from what was actually applied, not the unclamped value.
            self.envelope.reset(output);
        }

        self.previous_output_db = output;
        output
    }
}

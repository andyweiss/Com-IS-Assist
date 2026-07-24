use super::gain_computer::{GainComputer, GainComputerConfig};
use crate::ratio::RatioResult;

/// Target/tolerance policy for [`AutomixEngine`], matching the JSFX settings panel's framing
/// (`target_ratio_lu` defaults to its "Target Ratio (LU)" default, etc. — see
/// `Specs/TechnicalConcept.md` section 5).
#[derive(Debug, Clone, Copy)]
pub struct AutomixEngineConfig {
    pub target_ratio_lu: f64,
    pub max_tolerance_lu: f64,
    pub min_tolerance_lu: f64,
    pub max_gain_reduction_db: f64,
    /// How many dB the raw target nudges per LU of shortfall/excess, per tick. Deliberately a
    /// small incremental step, not a one-shot jump to the "needed" value — see the closed-loop
    /// note in `Specs/TechnicalConcept.md` section 4/5 for why a one-shot jump wouldn't
    /// converge cleanly (the 3s LUFS window smears the effect of any gain change in gradually).
    pub step_db_per_lu: f64,
}

impl Default for AutomixEngineConfig {
    fn default() -> Self {
        Self {
            target_ratio_lu: 3.0,
            max_tolerance_lu: 4.0,
            min_tolerance_lu: 8.0,
            max_gain_reduction_db: 24.0,
            step_db_per_lu: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutomixResult {
    pub gain_reduction_db: f64,
    pub gain_linear: f64,
}

/// Turns a [`RatioResult`] into a smoothed Bed gain-reduction value. See
/// `Specs/TechnicalConcept.md` section 5 for the full design and section 4 for the closed-loop
/// signal flow this is meant to run inside (the Bed loudness meter feeding `RatioEngine` must
/// observe the *already-gained* Bed signal, not the dry one).
pub struct AutomixEngine {
    config: AutomixEngineConfig,
    gain_computer: GainComputer,
    target_reduction_db: f64,
}

impl AutomixEngine {
    pub fn new(
        config: AutomixEngineConfig,
        gain_computer_config: GainComputerConfig,
        tick_seconds: f64,
    ) -> Self {
        Self {
            config,
            gain_computer: GainComputer::new(gain_computer_config, tick_seconds),
            target_reduction_db: 0.0,
        }
    }

    /// Call once per ~100ms tick with the latest `RatioEngine::update` result.
    pub fn process_tick(&mut self, ratio: RatioResult) -> AutomixResult {
        // `ratio.valid` is a one-way latch (meter-appropriate: never un-shows a value once
        // shown) so it doesn't mean "COM has signal right now" — during a long silence after the
        // first-ever speech burst, `valid` stays true but `ratio_lu` is a frozen, increasingly
        // stale number. React to it only when COM isn't currently silent, so a quiet gap between
        // dialogue bursts holds the current gain instead of chasing a stale ratio.
        if ratio.valid && !ratio.com_currently_silent {
            let com_hi_lim = self.config.target_ratio_lu + self.config.max_tolerance_lu;
            let com_lo_lim = self.config.target_ratio_lu - self.config.min_tolerance_lu;

            if ratio.ratio_lu < com_lo_lim {
                let shortfall = com_lo_lim - ratio.ratio_lu;
                self.target_reduction_db = (self.target_reduction_db
                    + shortfall * self.config.step_db_per_lu)
                    .min(self.config.max_gain_reduction_db);
            } else if ratio.ratio_lu > com_hi_lim {
                let excess = ratio.ratio_lu - com_hi_lim;
                self.target_reduction_db =
                    (self.target_reduction_db - excess * self.config.step_db_per_lu).max(0.0);
            }
            // Inside the dead-band: target_reduction_db is left unchanged (hold).
        }
        // !valid (insufficient COM history): target_reduction_db is left unchanged too.

        let smoothed_db = self.gain_computer.process_tick(self.target_reduction_db);
        AutomixResult {
            gain_reduction_db: smoothed_db,
            gain_linear: 10f64.powf(-smoothed_db / 20.0),
        }
    }

    /// Replaces the target/tolerance/ceiling config and the gain computer's attack/hold/release
    /// config, without resetting `target_reduction_db` or the gain computer's own in-progress
    /// envelope state - so a live parameter change (a VST3 host's parameter automation, or a
    /// dragged slider) actually takes effect on the next tick. Without this, a wrapper that only
    /// reads its parameters once at construction time would keep using whatever values were
    /// current back then forever after, no matter how the user later adjusts them.
    /// `target_reduction_db` is re-clamped against the new `max_gain_reduction_db` in case the
    /// ceiling was just lowered below the currently-applied reduction.
    pub fn set_config(&mut self, config: AutomixEngineConfig, gain_computer_config: GainComputerConfig) {
        self.config = config;
        self.target_reduction_db = self.target_reduction_db.min(config.max_gain_reduction_db);
        self.gain_computer.set_config(gain_computer_config);
    }

    /// Directly seeds the raw target, bypassing the ratio-driven policy for one tick — used by
    /// the VAD-onset fast trigger and the interview-passthrough override (both M3). The
    /// smoothing envelope still governs how quickly the *applied* gain actually moves toward it.
    pub fn seed_target_reduction_db(&mut self, value: f64) {
        self.target_reduction_db = value.clamp(0.0, self.config.max_gain_reduction_db);
    }

    pub fn current_gain_reduction_db(&self) -> f64 {
        self.gain_computer.value()
    }

    pub fn reset(&mut self) {
        self.target_reduction_db = 0.0;
        self.gain_computer.reset(0.0);
    }
}

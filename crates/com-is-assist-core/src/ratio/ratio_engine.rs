/// Result of one [`RatioEngine::update`] tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RatioResult {
    pub ratio_lu: f64,
    /// Once true, stays true forever (a meter-appropriate latch: "stop showing blank, show the
    /// last known value" — see the struct docs below). Consumers that make *control* decisions
    /// (not just display) should also check `com_currently_silent`, since `valid` alone doesn't
    /// mean COM has signal *right now*.
    pub valid: bool,
    /// True when COM's short-term loudness is at/below the valid-signal floor *this tick*,
    /// independent of the `valid` latch above. Added because `valid` alone caused
    /// `AutomixEngine` to keep reacting to a frozen, increasingly-stale `ratio_lu` during long
    /// silences after the first-ever speech burst — see `Specs/TechnicalConcept.md` section 5.
    pub com_currently_silent: bool,
}

/// Configuration for [`RatioEngine`]'s "hold when insufficient signal" gate.
#[derive(Debug, Clone, Copy)]
pub struct RatioEngineConfig {
    pub valid_signal_floor_db: f64,
    pub valid_signal_hold_ticks: i32,
}

impl Default for RatioEngineConfig {
    fn default() -> Self {
        Self {
            valid_signal_floor_db: -70.0,
            valid_signal_hold_ticks: 20,
        }
    }
}

/// Tracks the COM/IS loudness ratio with a JSFX-style "hold when insufficient
/// signal" gate (see `Specs/TechnicalConcept.md` section 2/4): rather than
/// reacting to every noise-floor short-term reading, the ratio only updates
/// once the dialogue (COM) channel has been above a floor for a sustained
/// number of ticks, and holds its last value otherwise. This replicates the
/// original meter's gating-histogram *policy*, not its internals.
#[derive(Debug, Clone, Copy)]
pub struct RatioEngine {
    config: RatioEngineConfig,
    consecutive_valid_ticks: i32,
    held_ratio_lu: f64,
    ever_valid: bool,
}

impl RatioEngine {
    pub fn new(config: RatioEngineConfig) -> Self {
        Self {
            config,
            consecutive_valid_ticks: 0,
            held_ratio_lu: 0.0,
            ever_valid: false,
        }
    }

    /// Call once per measurement tick (JSFX/spec default: ~100ms) with the
    /// latest short-term loudness of the Bed (IS) and Dialogue (COM) signals.
    pub fn update(&mut self, bed_short_term_db: f64, com_short_term_db: f64) -> RatioResult {
        let com_currently_silent = com_short_term_db <= self.config.valid_signal_floor_db;

        if !com_currently_silent {
            self.consecutive_valid_ticks += 1;
        } else {
            self.consecutive_valid_ticks = 0;
        }

        if self.consecutive_valid_ticks >= self.config.valid_signal_hold_ticks {
            self.held_ratio_lu = com_short_term_db - bed_short_term_db;
            self.ever_valid = true;
        }

        RatioResult {
            ratio_lu: self.held_ratio_lu,
            valid: self.ever_valid,
            com_currently_silent,
        }
    }

    pub fn reset(&mut self) {
        self.consecutive_valid_ticks = 0;
        self.held_ratio_lu = 0.0;
        self.ever_valid = false;
    }
}

impl Default for RatioEngine {
    fn default() -> Self {
        Self::new(RatioEngineConfig::default())
    }
}

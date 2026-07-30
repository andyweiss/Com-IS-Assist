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
    /// Whether the *previous* tick's `RatioResult::voice_active` was true - used only to detect
    /// the false→true onset transition (see `process_tick`'s fast-trigger doc comment).
    was_voice_active: bool,
    /// The most recent `target_reduction_db` while voice was active - i.e. wherever the
    /// ratio-driven policy last left it before COM went quiet. Re-seeded on the next onset - see
    /// `process_tick`.
    last_active_target_reduction_db: f64,
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
            was_voice_active: false,
            last_active_target_reduction_db: 0.0,
        }
    }

    /// Call once per ~100ms tick with the latest `RatioEngine::update` result.
    pub fn process_tick(&mut self, ratio: RatioResult) -> AutomixResult {
        let voice_onset = ratio.voice_active && !self.was_voice_active;
        self.was_voice_active = ratio.voice_active;

        // No voice detected on COM right now: there's nothing to duck Bed *for*, so release back
        // toward unity (at whatever `release_seconds`/"Recovery Time" is configured) rather than
        // holding whatever reduction happened to be applied when COM stopped talking. This is
        // deliberately unconditional on `ratio.valid` - even before a ratio has ever been
        // established, "no voice" means "nothing to react to," the same as after one has. Setting
        // the raw target straight to `0.0` and letting `gain_computer`'s own release envelope
        // (below) ease the *applied* gain back smoothly is exactly the same mechanism already
        // used for "ratio comfortably above target" (see the `com_hi_lim` branch) - no new timing
        // knob needed.
        if !ratio.voice_active {
            self.target_reduction_db = 0.0;
        } else {
            // VAD-onset fast trigger (`Specs/TechnicalConcept.md` section 5/6): the instant voice
            // resumes after a gap, immediately jump the raw target back to wherever it was
            // converging to *before* COM went quiet, rather than starting the ratio-driven
            // "nudge, don't jump" ramp from `0.0` again. `RatioEngine`'s own `held_ratio_lu` also
            // needs a few ticks after an onset to refresh with fresh audio (short-term loudness
            // has real integration time, on top of `valid_signal_hold_ticks`'s own small
            // debounce) - without this, Bed would sit un-ducked for that entire window on *every*
            // resumed utterance, not just the very first one in a session. This is a genuine
            // guess, not a measurement - if the correct reduction has actually changed since the
            // last utterance, the ratio-driven policy below corrects it within a few ticks, same
            // as it would from any other starting point.
            if voice_onset {
                self.target_reduction_db = self.last_active_target_reduction_db;
            }

            if ratio.valid {
                // `ratio.valid` is a one-way latch (meter-appropriate: never un-shows a value
                // once shown) so on its own it doesn't mean "COM has signal right now" — during a
                // long gap after the first-ever speech burst, `valid` stays true but `ratio_lu` is
                // a frozen, increasingly stale number. The `!voice_active` branch above already
                // handles that case (COM having gone quiet); this branch only runs once real
                // voice-activity detection *also* confirms speech is actually present this tick.
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
            // voice_active but !valid (insufficient COM history to trust ratio_lu yet): hold
            // (possibly at the just-fast-triggered value from this same tick, if `voice_onset`).

            self.last_active_target_reduction_db = self.target_reduction_db;
        }

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

    /// Directly seeds the raw target, bypassing the ratio-driven policy for one tick — for the
    /// interview-passthrough override (M3, not yet built). The VAD-onset fast trigger this was
    /// originally written for is now handled automatically inside `process_tick` itself (see its
    /// `voice_onset` handling) and no longer needs a caller to invoke this explicitly.
    ///
    /// **Known caveat for the not-yet-built interview-passthrough override**: that feature's own
    /// design (`Specs/TechnicalConcept.md` section 5) forces reduction toward `0.0` specifically
    /// when COM has *no* voice (and Bed does) - i.e. exactly the condition under which
    /// `process_tick`'s `!ratio.voice_active` branch *also* unconditionally resets
    /// `target_reduction_db` to `0.0` on every tick. A future caller seeding a value while COM's
    /// `voice_active` is false would have that seed immediately overwritten before this method
    /// even returns control to `process_tick`'s caller. Whichever mechanism implements that
    /// override will need its own explicit bypass of the `!voice_active` release branch (e.g. a
    /// dedicated override-active flag checked ahead of it), not a bare call to this method - not
    /// solved here since the override itself doesn't exist yet.
    ///
    /// The smoothing envelope still governs how quickly the *applied* gain actually moves toward
    /// whatever value is seeded.
    pub fn seed_target_reduction_db(&mut self, value: f64) {
        self.target_reduction_db = value.clamp(0.0, self.config.max_gain_reduction_db);
    }

    pub fn current_gain_reduction_db(&self) -> f64 {
        self.gain_computer.value()
    }

    pub fn reset(&mut self) {
        self.target_reduction_db = 0.0;
        self.was_voice_active = false;
        self.last_active_target_reduction_db = 0.0;
        self.gain_computer.reset(0.0);
    }
}

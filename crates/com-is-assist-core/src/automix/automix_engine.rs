use super::loop_bank::{LoopBank, LoopBankConfig, LoopContributions};
use super::mix_state::MixState;
use crate::loudness::Ebur128Meter;

/// The feed-forward identity at the heart of the design: reducing the Bed by 1 dB raises the
/// COM/IS ratio by exactly 1 LU, so the reduction needed to put the ratio on target is simply
/// `target - (COM - BED)`. Exact only because both readings come from the **dry** signals.
///
/// `None` when either side is at the silence sentinel, where the "ratio" is not a real quantity —
/// propagating it would ask for an enormous reduction on the strength of a placeholder number.
fn required_reduction(target_ratio_lu: f64, bed_lufs: f64, dialogue_lufs: f64) -> Option<f64> {
    if bed_lufs <= Ebur128Meter::NEGATIVE_INFINITY_DB || dialogue_lufs <= Ebur128Meter::NEGATIVE_INFINITY_DB {
        return None;
    }
    Some(target_ratio_lu - (dialogue_lufs - bed_lufs))
}

/// Target policy for [`AutomixEngine`]. See `Specs/TechnicalConcept.md` section 5 for the state
/// machine (5.1) and the multi-loop control law (5.2) these values feed.
#[derive(Debug, Clone, Copy)]
pub struct AutomixEngineConfig {
    /// `UI.md`'s "Com/IS distance": how many LU COM should sit *above* IS while the commentator is
    /// talking and the Bed has no voice of its own ([`MixState::DuckToTarget`]).
    pub target_ratio_lu: f64,
    /// The higher target for the double-talk / "over-voice" case ([`MixState::DuckToOvervoice`]):
    /// both COM *and* IS carry voice, so COM needs more headroom above the Bed to stay
    /// intelligible. An **absolute** target, not an offset on `target_ratio_lu`, so the two cases
    /// can be dialed in independently by ear.
    pub overvoice_ratio_lu: f64,
    pub max_gain_reduction_db: f64,
    /// The "Speed" macro — scales every stage's ballistics together. This is the *only* timing
    /// control, replacing the five separate time parameters (`adaptation`/`attack`/`hold`/
    /// `release`/`interview-release`) the single-loop design needed: with the multi-loop cascade
    /// the effective attack and release are emergent, so the individual times had nothing
    /// meaningful left to set.
    pub speed: f64,
}

impl Default for AutomixEngineConfig {
    fn default() -> Self {
        Self {
            target_ratio_lu: 3.0,
            overvoice_ratio_lu: 8.0,
            max_gain_reduction_db: 24.0,
            speed: 1.0,
        }
    }
}

/// The loudness readings one control tick works from, all measured **feed-forward on the dry
/// signals** — see [`AutomixEngine::process_tick`].
///
/// Note the asymmetry: the Bed is read at several speeds, the Dialogue at only one. That is
/// deliberate and is what the stages actually differ in.
///
/// **Why only the Bed gets fast detectors.** `ratio = COM − BED`, so `required = target − ratio`:
/// if COM gets *louder* the ratio rises and *less* ducking is needed, not more. The event that
/// genuinely demands a fast response is therefore a **Bed** surge — a crowd roar burying the
/// commentator — not a COM one. Driving the fast stages from COM as well was tried and was wrong
/// twice over: at 50ms a speech signal's "level" is its syllable envelope, so the stage tracked
/// vowels and modulated the Bed at syllable rate; and because a 3s window over speech-with-gaps
/// measures a systematically quieter COM than a 400ms window during speech, the stages disagreed
/// by a constant offset and sat fighting each other (measured: the mid stage parked at −2 to −5 dB
/// while the fast stage repeatedly saturated its +4 dB clamp).
///
/// Reading COM at one stable speed for every stage removes both problems: the stages now measure
/// the *same* quantity and differ only in how quickly they see the Bed move, so they agree in
/// steady state and separate only during a Bed transient — which is exactly the intended division
/// of labour.
#[derive(Debug, Clone, Copy)]
pub struct LoudnessSnapshot {
    /// Bed at the 3s short-term window — the slow stage's detector.
    pub bed_short_term_lufs: f64,
    /// Bed at the 400ms momentary window — the mid stage's detector.
    pub bed_momentary_lufs: f64,
    /// COM at the 3s short-term window. The single Dialogue reference every stage works against.
    pub dialogue_short_term_lufs: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutomixResult {
    pub gain_reduction_db: f64,
    pub gain_linear: f64,
    /// Which state produced this tick's result — surfaced so wrappers can display it and tests can
    /// assert on the state machine directly rather than inferring it from gain values.
    pub state: MixState,
    pub contributions: LoopContributions,
}

/// Decides *what* the mixer should be doing (the four-state machine) and hands the question of
/// *how* to get there to the multi-loop [`LoopBank`].
///
/// The split matters: the state machine is the part that was already right, and it stays exactly as
/// it was. Everything underneath it — a single feedback loop with five time constants — was
/// replaced by the feed-forward cascade, because no amount of tuning fixes a topology whose
/// measurement lags its actuation by three seconds.
pub struct AutomixEngine {
    config: AutomixEngineConfig,
    loops: LoopBank,
    previous_state: MixState,
}

impl AutomixEngine {
    pub fn new(config: AutomixEngineConfig) -> Self {
        Self {
            loops: LoopBank::new(Self::loop_config(&config)),
            config,
            previous_state: MixState::ReleaseToUnity,
        }
    }

    fn loop_config(config: &AutomixEngineConfig) -> LoopBankConfig {
        LoopBankConfig {
            speed: config.speed,
            max_gain_reduction_db: config.max_gain_reduction_db,
            ..LoopBankConfig::default()
        }
    }

    /// Call once per ~100ms control tick with dry-signal loudness at both R128 windows, the two
    /// voice-activity flags, and the elapsed time since the previous call.
    ///
    /// The required reduction at each time scale is computed directly:
    /// `required = target_ratio − (COM − BED)`. That identity holds because reducing the Bed by
    /// 1 dB raises the ratio by exactly 1 LU, and it is exact only because both readings come from
    /// the **dry** signals. Measuring the already-ducked Bed (as this engine used to) turns the
    /// same expression into a feedback loop that cannot see its own effect for three seconds.
    pub fn process_tick(
        &mut self,
        loudness: LoudnessSnapshot,
        com_voice_active: bool,
        is_voice_active: bool,
        dt_seconds: f64,
    ) -> AutomixResult {
        let state = MixState::from_vad(com_voice_active, is_voice_active);

        if state.is_ducking() {
            // Note there is deliberately **no** voice-onset fast trigger here. The feedback design
            // needed one: after a gap its measurement was stale, so it had to be told where to
            // resume from, and that seeding produced an instantaneous ~19dB jump in the applied
            // gain. Feed-forward has nothing stale to compensate for - the dry levels are correct
            // on the very first tick back - and the cascade covers the onset on its own: the mid
            // stage engages within ~150ms and the fast stage within milliseconds, while the slow
            // stage takes over the steady state behind them. Measured on real commentary, removing
            // the trigger cut the largest single-tick gain movement from 19.4dB to 8.0dB with no
            // loss of onset response.
            let target = match state {
                MixState::DuckToOvervoice => self.config.overvoice_ratio_lu,
                _ => self.config.target_ratio_lu,
            };
            let required_slow = required_reduction(
                target,
                loudness.bed_short_term_lufs,
                loudness.dialogue_short_term_lufs,
            );
            let required_mid = required_reduction(
                target,
                loudness.bed_momentary_lufs,
                loudness.dialogue_short_term_lufs,
            );
            self.loops.update_slow_mid(required_slow, required_mid, dt_seconds);
        } else {
            self.loops
                .update_release(state == MixState::InterviewPassthrough, dt_seconds);
        }

        self.previous_state = state;
        self.result(state)
    }

    /// Steps the fast stage between control ticks, at audio sub-chunk rate, from the fast
    /// K-weighted **Bed** detector against the stable COM reference (see [`LoudnessSnapshot`] for
    /// why COM is not read fast). Uses the state the last control tick resolved to — the state
    /// machine runs on VAD, which does not change meaningfully within one audio block.
    pub fn process_fast(&mut self, bed_fast_lufs: f64, dialogue_reference_lufs: f64, dt_seconds: f64) -> f64 {
        let required = if self.previous_state.is_ducking() {
            let target = match self.previous_state {
                MixState::DuckToOvervoice => self.config.overvoice_ratio_lu,
                _ => self.config.target_ratio_lu,
            };
            required_reduction(target, bed_fast_lufs, dialogue_reference_lufs)
        } else {
            None
        };
        self.loops.update_fast(required, dt_seconds);
        self.loops.total_reduction_db()
    }

    /// The ratio target the engine is currently aiming at, for metering.
    pub fn active_target_ratio_lu(&self) -> f64 {
        match self.previous_state {
            MixState::DuckToOvervoice => self.config.overvoice_ratio_lu,
            _ => self.config.target_ratio_lu,
        }
    }

    fn result(&self, state: MixState) -> AutomixResult {
        let contributions = self.loops.contributions();
        AutomixResult {
            gain_reduction_db: contributions.total_db,
            gain_linear: 10f64.powf(-contributions.total_db / 20.0),
            state,
            contributions,
        }
    }

    /// Replaces the config in place without disturbing any in-flight stage value, so host
    /// automation (or a dragged slider) takes effect on the next step.
    pub fn set_config(&mut self, config: AutomixEngineConfig) {
        self.config = config;
        self.loops.set_config(Self::loop_config(&config));
    }

    pub fn current_gain_reduction_db(&self) -> f64 {
        self.loops.total_reduction_db()
    }

    pub fn contributions(&self) -> LoopContributions {
        self.loops.contributions()
    }

    /// The state the most recent `process_tick` resolved to (`ReleaseToUnity` before the first).
    pub fn current_state(&self) -> MixState {
        self.previous_state
    }

    pub fn reset(&mut self) {
        self.loops.reset();
        self.previous_state = MixState::ReleaseToUnity;
    }
}

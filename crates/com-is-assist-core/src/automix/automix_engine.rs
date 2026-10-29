use super::gain_loop::{GainLoop, GainLoopConfig, DEFAULT_ATTACK_SECONDS, DEFAULT_RELEASE_SECONDS};
use super::mix_state::MixState;
use crate::loudness::{BedLayout, Ebur128Meter};

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
/// machine (5.1) and the control law (5.2) these values feed.
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
    /// Guard band for COM voice detection: speech reported on COM is **ignored** while COM's own
    /// momentary loudness sits below this, in LUFS.
    ///
    /// The detector answers "is this speech?", not "is this speech *on this microphone*". A
    /// commentary mic picks up the programme bed as crosstalk, and that spill is real speech - so
    /// a VAD will quite correctly fire on it, and the Bed then ducks itself. A level gate is the
    /// natural discriminator, because spill is reliably far quieter than the commentator; what it
    /// cannot be is a *loudness* judgement, which is why this sits alongside the VAD rather than
    /// replacing it.
    ///
    /// Ships **engaged** at -32 LUFS rather than parked at the bottom of its range, where it would
    /// protect nothing. Useful settings are typically 20-30 dB below the commentator's own
    /// momentary level.
    pub com_gate_lufs: f64,
    /// **Attack**, in seconds: the time constant with which the reduction moves toward a larger
    /// answer. Default 2.5s - see [`DEFAULT_ATTACK_SECONDS`] for why it is long.
    pub attack_seconds: f64,
    /// **Release**, in seconds: the time constant with which the reduction lets go. Default 3s.
    ///
    /// The release ease-in after ducking stops (`RELEASE_HOLD_SECONDS`) and the faster interview
    /// recovery are fixed proportions on top of this, not separate settings.
    pub release_seconds: f64,
    /// How much of the computed gain reduction each group of Bed channels receives - see
    /// [`DuckDepth`].
    pub duck_depth: DuckDepth,
}

/// The widest Bed this project supports (`BedLayout`): 5.1.4 and 7.1.2.
pub const MAX_BED_CHANNELS: usize = 10;

/// How much of the computed gain reduction each group of Bed channels actually takes, as a factor
/// from `1.0` (the reduction in full - 6 dB of ducking reduces those channels by 6 dB) down to
/// `0.0` (not ducked at all). Values scale the **decibels** linearly, so `0.5` against 6 dB of
/// ducking is 3 dB.
///
/// This is a *distribution* control, not a second gain stage: the reduction itself is still
/// computed once, from the whole Bed's loudness, by the state machine and the gain loop. All it
/// changes is how that one answer is spread - which is why leaving every group at 1.0 reproduces
/// the pre-feature behaviour exactly.
///
/// It exists because the Bed is not homogeneous. Ducking the surrounds as hard as the front throws
/// away atmosphere that was never competing with the commentator, and the LFE carries no
/// intelligibility to protect at all; holding those back while the front ducks fully keeps the
/// commentary clear without flattening the scene around it.
///
/// **Grouped, not per channel, and deliberately so.** Left and Right must duck by the same amount
/// or the difference is heard as the image sliding sideways; the same goes for the surround, side
/// and height pairs. Independent per-channel settings make that mistake easy to reach and buy
/// nothing, since no real use for an asymmetric duck has come up.
///
/// A group only resolves to anything on a layout that has those channels (see the field docs); on
/// the others it is carried but never read.
#[derive(Debug, Clone, Copy)]
pub struct DuckDepth {
    /// Left and Right together.
    pub front_lr: f64,
    pub center: f64,
    pub lfe: f64,
    /// Left Surround and Right Surround together - VST3's `Ls`/`Rs` on every layout, which on
    /// 7.1.2 are the rear pair.
    pub surround: f64,
    /// Side Left and Side Right together (7.1.2 only).
    pub side: f64,
    /// Top Front Left and Top Front Right together (5.1.4 only).
    pub top_front: f64,
    /// Top Rear Left and Top Rear Right together (5.1.4 only).
    pub top_rear: f64,
    /// Top Side Left and Top Side Right together (7.1.2 only).
    pub top_side: f64,
}

impl Default for DuckDepth {
    fn default() -> Self {
        Self {
            front_lr: 1.0,
            center: 1.0,
            lfe: 1.0,
            surround: 1.0,
            side: 1.0,
            top_front: 1.0,
            top_rear: 1.0,
            top_side: 1.0,
        }
    }
}

impl DuckDepth {
    /// Resolves the groups to a factor per channel **position** for a Bed of `bed_channels`, taken
    /// as [`BedLayout::from_channel_count`] reads it. See [`Self::per_channel_for`].
    pub fn per_channel(&self, bed_channels: u32) -> [f64; MAX_BED_CHANNELS] {
        match BedLayout::from_channel_count(bed_channels) {
            Ok(layout) => self.per_channel_for(layout),
            Err(_) => [1.0; MAX_BED_CHANNELS],
        }
    }

    /// Resolves the groups to a factor per channel **position** for `layout`, against the same map
    /// the loudness meters use ([`BedLayout::channel_map`]).
    ///
    /// Doing it here rather than in each wrapper is what keeps the group names honest: which role
    /// sits at a given stream position depends on the channel count (position 2 is Center on a
    /// 3/5/6-channel Bed but Left Surround on a 4-channel one), so a wrapper exposing positional
    /// settings would be mislabelling them on every layout but the widest. Positions past the
    /// layout's width resolve to 1.0 and are never read.
    pub fn per_channel_for(&self, layout: BedLayout) -> [f64; MAX_BED_CHANNELS] {
        use ebur128::Channel;

        let mut out = [1.0; MAX_BED_CHANNELS];
        for (slot, channel) in out.iter_mut().zip(layout.channel_map()) {
            *slot = match channel {
                Channel::Left | Channel::Right => self.front_lr,
                Channel::Center => self.center,
                // `bed_channel_map` marks the 6-channel layout's LFE as `Unused`: BS.1770 excludes
                // it from the loudness sum, which is exactly why it is also the channel with no
                // intelligibility to protect.
                Channel::Unused => self.lfe,
                Channel::Mp090 | Channel::Mm090 => self.side,
                Channel::Up030 | Channel::Um030 => self.top_front,
                Channel::Up110 | Channel::Um110 => self.top_rear,
                Channel::Up090 | Channel::Um090 => self.top_side,
                // Ls/Rs, wherever the layout puts them (M±110 on 5.1, M±135 on 7.1.2).
                _ => self.surround,
            };
        }
        out
    }
}

impl Default for AutomixEngineConfig {
    fn default() -> Self {
        Self {
            target_ratio_lu: 5.0,
            overvoice_ratio_lu: 13.0,
            max_gain_reduction_db: 24.0,
            // Not fully open (-70, the bottom of the control's range). The guard band only earns
            // its keep when it is actually above the crosstalk, and -32 is a working starting
            // point on real commentary: comfortably under a commentator's own momentary level,
            // comfortably over the bed's bleed into the mic.
            com_gate_lufs: -32.0,
            attack_seconds: DEFAULT_ATTACK_SECONDS,
            release_seconds: DEFAULT_RELEASE_SECONDS,
            duck_depth: DuckDepth::default(),
        }
    }
}

/// The loudness readings one step works from, both measured **feed-forward on the dry signals** on
/// R128's 3s short-term window - see [`AutomixEngine::step`].
///
/// Short-term on *both* sides, deliberately. Reading either side faster was tried, in the
/// three-stage cascade this replaced: COM read quickly tracks the syllable envelope of speech, and a quickly-read Bed
/// on a fluctuating programme gets peak-followed, so the mixer reduces more than the balance needs
/// and keeps moving. One window on both sides measures one quantity - the programme balance.
#[derive(Debug, Clone, Copy)]
pub struct LoudnessSnapshot {
    pub bed_short_term_lufs: f64,
    pub dialogue_short_term_lufs: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutomixResult {
    pub gain_reduction_db: f64,
    pub gain_linear: f64,
    /// Which state produced this step's result — surfaced so wrappers can display it and tests can
    /// assert on the state machine directly rather than inferring it from gain values.
    pub state: MixState,
}

/// Decides *what* the mixer should be doing (the four-state machine) and hands the question of
/// *how* to get there to the [`GainLoop`].
///
/// The split matters: the state machine is the part that was already right, and it stays exactly as
/// it was. Underneath it, an original feedback loop with five time constants was replaced by a
/// feed-forward one, because no amount of tuning fixes a topology whose measurement lags its
/// actuation by three seconds.
pub struct AutomixEngine {
    config: AutomixEngineConfig,
    gain_loop: GainLoop,
    previous_state: MixState,
}

impl AutomixEngine {
    pub fn new(config: AutomixEngineConfig) -> Self {
        Self {
            gain_loop: GainLoop::new(Self::loop_config(&config)),
            config,
            previous_state: MixState::ReleaseToUnity,
        }
    }

    fn loop_config(config: &AutomixEngineConfig) -> GainLoopConfig {
        GainLoopConfig {
            attack_seconds: config.attack_seconds,
            release_seconds: config.release_seconds,
            max_gain_reduction_db: config.max_gain_reduction_db,
        }
    }

    /// Advances the mixer by `dt_seconds`, given dry-signal short-term loudness on both sides and
    /// the two voice-activity flags. Wrappers call it once per audio sub-chunk (~1.3ms), so a state
    /// change takes effect at once and the gain follows a smooth curve; any step size works,
    /// because the loop's ballistics are exact for any `dt`.
    ///
    /// The required reduction is computed directly: `required = target_ratio − (COM − BED)`. That identity holds because reducing the Bed by
    /// 1 dB raises the ratio by exactly 1 LU, and it is exact only because both readings come from
    /// the **dry** signals. Measuring the already-ducked Bed (as this engine used to) turns the
    /// same expression into a feedback loop that cannot see its own effect for three seconds.
    pub fn step(
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
            // on the very first step back, so the loop's own attack is all the onset needs.
            // Measured on real commentary, removing the trigger cut the largest single-tick gain
            // movement from 19.4dB to 8.0dB with no loss of onset response.
            let target = match state {
                MixState::DuckToOvervoice => self.config.overvoice_ratio_lu,
                _ => self.config.target_ratio_lu,
            };
            let required = required_reduction(
                target,
                loudness.bed_short_term_lufs,
                loudness.dialogue_short_term_lufs,
            );
            self.gain_loop.update(required, dt_seconds);
        } else {
            self.gain_loop
                .update_release(state == MixState::InterviewPassthrough, dt_seconds);
        }

        self.previous_state = state;
        self.result(state)
    }

    /// The ratio target the engine is currently aiming at, for metering.
    pub fn active_target_ratio_lu(&self) -> f64 {
        match self.previous_state {
            MixState::DuckToOvervoice => self.config.overvoice_ratio_lu,
            _ => self.config.target_ratio_lu,
        }
    }

    fn result(&self, state: MixState) -> AutomixResult {
        let gain_reduction_db = self.gain_loop.reduction_db();
        AutomixResult { gain_reduction_db, gain_linear: 10f64.powf(-gain_reduction_db / 20.0), state }
    }

    /// Replaces the config in place without disturbing the in-flight reduction, so host
    /// automation (or a dragged slider) takes effect on the next step.
    pub fn set_config(&mut self, config: AutomixEngineConfig) {
        self.config = config;
        self.gain_loop.set_config(Self::loop_config(&config));
    }

    pub fn current_gain_reduction_db(&self) -> f64 {
        self.gain_loop.reduction_db()
    }

    /// The state the most recent `step` resolved to (`ReleaseToUnity` before the first).
    pub fn current_state(&self) -> MixState {
        self.previous_state
    }

    pub fn reset(&mut self) {
        self.gain_loop.reset();
        self.previous_state = MixState::ReleaseToUnity;
    }
}

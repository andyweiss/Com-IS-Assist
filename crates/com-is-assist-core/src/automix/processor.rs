use ebur128::Error as Ebur128Error;

use crate::automix::{
    apply_ramped_gain_per_channel, AutomixEngine, AutomixEngineConfig, KWeightedLoudnessMeter,
    LoudnessSnapshot, MixState, MAX_BED_CHANNELS,
};
use crate::loudness::{BedLayout, ConfigError, Ebur128Meter};

/// The gain computer is stepped, and a new gain applied, once per this many frames rather than once
/// per audio block. At 48kHz that is ~1.3ms, so the gain follows a smooth curve whatever buffer
/// size the host uses (often 10-20ms), and a voice-detector state change takes effect at once.
///
/// This replaced a ~100ms control tick. The tick existed because querying the `ebur128` crate's
/// windows was expensive; `KWeightedLoudnessMeter` made the queries O(1), and stepping only on the
/// tick left the gain to change in one jump every 100ms, ramped over a single sub-chunk.
const SUBCHUNK_FRAMES: usize = 64;

/// Bundles the loudness detectors and the gain computer that every
/// wrapper (the VST3 plugin and `com-is-assist-offline`) needs byte-for-byte
/// identically. This is what actually enforces "no DSP divergence between wrappers"
/// (`Specs/TechnicalConcept.md` section 8): there is exactly one implementation of how audio gets
/// measured and gained, and every wrapper calls into it.
///
/// **Feed-forward** (section 5.2): every detector observes the *dry* signal. The previous design
/// measured the already-ducked Bed, which made the control a feedback loop that could not see its
/// own effect for the length of the short-term window — the source of the windup and pumping that
/// a succession of control laws failed to tune away. Measuring dry makes the required reduction
/// directly computable instead.
///
/// Deliberately decoupled from how audio is chunked or how often each side is fed: Bed and Dialogue
/// can arrive together every call (a VST3 host's synchronous `process()`) or independently on their
/// own schedules, which is what `com-is-assist-offline` does when the two WAVs differ in length.
pub struct AutomixProcessor {
    bed_layout: BedLayout,
    bed_channels: u32,
    sample_rate: u32,

    /// Dry Bed and Dialogue. The gain loop reads their 3s short-term windows; the 400ms momentary
    /// windows feed the meters and the COM guard band.
    ///
    /// These were `Ebur128Meter`s until the real-time cost was measured. That crate recomputes a
    /// window's sum of squares on every query, which cost ~700us for short-term on a 6-channel bed
    /// and put a ~1.3ms spike on one audio block in seventy-five - more than a 64-sample budget
    /// allows, by itself. `KWeightedLoudnessMeter` keeps the same rectangular windows but maintains
    /// them incrementally, so a query is O(1). `Ebur128Meter` remains the conformance reference
    /// the two are tested against.
    bed_meter: KWeightedLoudnessMeter,
    dialogue_meter: KWeightedLoudnessMeter,
    automix_engine: AutomixEngine,

    /// The gain actually applied to the most recent sample **of each channel**, so the next
    /// sub-chunk can ramp from it rather than stepping. Per-channel since
    /// `AutomixEngineConfig::duck_depth` lets the channels sit at different gains.
    current_gain: [f32; MAX_BED_CHANNELS],
    automix_enabled: bool,

    /// The voice flags in force, as last published by [`Self::set_voice_activity`] - COM's after
    /// the guard band.
    com_voice_active: bool,
    is_voice_active: bool,

    applied_gain_reduction_db: f64,
    control_ratio_lu: f64,
    mix_state: MixState,

    /// See [`AutomixEngineConfig::com_gate_lufs`]. Held here rather than in the engine because the
    /// gate needs COM's measured level, which lives on this side.
    com_gate_lufs: f64,
    /// [`DuckDepth`] already resolved against this Bed's channel count, so the hot path never
    /// touches the channel map. Held here rather than in the engine because it shapes how one
    /// computed reduction is *applied*, not how it is decided.
    duck_depth: [f64; MAX_BED_CHANNELS],
}

impl AutomixProcessor {
    /// A Bed of `bed_channels`, taken as [`BedLayout::from_channel_count`] reads it - so 10 is
    /// 5.1.4. Use [`Self::with_layout`] for 7.1.2.
    pub fn new(
        bed_channels: u32,
        sample_rate: u32,
        automix_config: AutomixEngineConfig,
    ) -> Result<Self, ConfigError> {
        Ok(Self::with_layout(BedLayout::from_channel_count(bed_channels)?, sample_rate, automix_config))
    }

    pub fn with_layout(bed_layout: BedLayout, sample_rate: u32, automix_config: AutomixEngineConfig) -> Self {
        let bed_map = bed_layout.channel_map();
        let bed_channels = bed_map.len() as u32;
        Self {
            bed_layout,
            bed_channels,
            sample_rate,
            bed_meter: KWeightedLoudnessMeter::new(&bed_map, sample_rate),
            dialogue_meter: KWeightedLoudnessMeter::new(&Ebur128Meter::dialogue_channel_map(), sample_rate),
            automix_engine: AutomixEngine::new(automix_config),
            current_gain: [1.0; MAX_BED_CHANNELS],
            automix_enabled: true,
            com_voice_active: false,
            is_voice_active: false,
            applied_gain_reduction_db: 0.0,
            control_ratio_lu: 0.0,
            mix_state: MixState::ReleaseToUnity,
            com_gate_lufs: automix_config.com_gate_lufs,
            duck_depth: automix_config.duck_depth.per_channel_for(bed_layout),
        }
    }

    pub fn bed_channels(&self) -> u32 {
        self.bed_channels
    }

    /// Refreshes the live-adjustable config without resetting any accumulated state. Safe to call
    /// every block - it is a couple of struct copies, not a rebuild.
    pub fn set_config(&mut self, automix_config: AutomixEngineConfig) {
        self.com_gate_lufs = automix_config.com_gate_lufs;
        self.duck_depth = automix_config.duck_depth.per_channel_for(self.bed_layout);
        self.automix_engine.set_config(automix_config);
    }

    /// `false` bypasses automix: Bed passes at unity gain. Metering keeps running.
    pub fn set_automix_enabled(&mut self, enabled: bool) {
        self.automix_enabled = enabled;
    }

    /// Feeds one chunk of *dry* Dialogue into the Dialogue meter. Call it before
    /// [`Self::process_bed`] for the same block, so the Bed is gained against current COM levels.
    /// Dialogue is never gained and never delayed, so there is no equivalent of `process_bed` on
    /// this side - the caller forwards its own buffer untouched.
    pub fn feed_dialogue(&mut self, dry_dialogue: &[f32]) -> Result<(), Ebur128Error> {
        self.dialogue_meter.push_frames(dry_dialogue);
        Ok(())
    }

    /// The Bed signal path, in place: measures the dry signal, then steps the gain computer and
    /// applies the resulting gain in sub-chunks.
    ///
    /// Order matters and is the whole feed-forward idea: audio is *measured* before any gain is
    /// applied to it, so the control value that reaches a given sample was derived from that same
    /// sample rather than from one three seconds old. Nothing is buffered or held back, so this
    /// path adds exactly zero latency - a property broadcast deployment depends on (see
    /// `Specs/TechnicalConcept.md` section 7).
    pub fn process_bed(&mut self, bed: &mut [f32]) -> Result<(), Ebur128Error> {
        let channels = self.bed_channels as usize;
        if channels == 0 || bed.is_empty() {
            return Ok(());
        }

        for subchunk in bed.chunks_mut(SUBCHUNK_FRAMES * channels) {
            // Dry measurement, before anything is applied.
            self.bed_meter.push_frames(subchunk);

            let loudness = LoudnessSnapshot {
                bed_short_term_lufs: self.bed_meter.short_term_loudness_db(),
                dialogue_short_term_lufs: self.dialogue_meter.short_term_loudness_db(),
            };
            self.control_ratio_lu = loudness.dialogue_short_term_lufs - loudness.bed_short_term_lufs;

            // The final sub-chunk of a block is usually short, so derive dt from its real length
            // rather than assuming a full one - the loop's ballistics are in seconds.
            let dt = (subchunk.len() / channels) as f64 / self.sample_rate as f64;
            let result = self.automix_engine.step(loudness, self.com_voice_active, self.is_voice_active, dt);
            self.mix_state = result.state;
            let reduction_db = result.gain_reduction_db;
            self.applied_gain_reduction_db = reduction_db;

            // One reduction, distributed across the channels by `duck_depth`. Scaling in **dB**
            // rather than scaling the linear gain is what makes the control mean what it says: at
            // 0.5 a channel gets half the *decibels*, so the relationship between channels holds
            // at every depth instead of drifting with the size of the reduction.
            let mut target_gain = [1.0_f32; MAX_BED_CHANNELS];
            if self.automix_enabled {
                for (gain, depth) in target_gain.iter_mut().zip(self.duck_depth) {
                    *gain = 10f64.powf(-(reduction_db * depth) / 20.0) as f32;
                }
            }

            apply_ramped_gain_per_channel(subchunk, self.bed_channels, &self.current_gain, &target_gain);
            self.current_gain = target_gain;
        }

        Ok(())
    }

    /// Publishes the voice detectors' current answer. Call this every block, before
    /// [`Self::process_bed`], so a state change takes effect from that block's first sub-chunk.
    ///
    /// `com_voice_active`/`is_voice_active` come from the caller's two `SileroVad` instances (see
    /// `crate::voice_activity`); together they select the [`MixState`]. COM passes through the guard
    /// band here - see [`AutomixEngineConfig::com_gate_lufs`]. Momentary (400ms) rather than
    /// short-term, so the gate tracks the talker rather than lagging seconds behind them.
    pub fn set_voice_activity(&mut self, com_voice_active: bool, is_voice_active: bool) {
        self.com_voice_active =
            com_voice_active && self.dialogue_meter.momentary_loudness_db() >= self.com_gate_lufs;
        self.is_voice_active = is_voice_active;
        self.mix_state = MixState::from_vad(self.com_voice_active, self.is_voice_active);
    }

    // ---- metering ----------------------------------------------------------------------------

    pub fn applied_gain_reduction_db(&self) -> f64 {
        self.applied_gain_reduction_db
    }

    pub fn mix_state(&self) -> MixState {
        self.mix_state
    }

    /// Whether COM voice is currently considered present, *after* the guard band. This is what
    /// actually drives the state machine, so it is what a GUI indicator should show - a raw
    /// detector flag would light while the gate was suppressing it.
    pub fn com_voice_gated(&self) -> bool {
        self.com_voice_active
    }

    /// The COM/IS ratio the gain loop is actually achieving: the dry ratio plus whatever
    /// reduction is currently applied. This is the number to compare against the target, and it is
    /// meaningful in a way the old feedback design's equivalent was not - there, the measured ratio
    /// already contained the reduction, so "ratio vs target" and "how much more to do" were the
    /// same quantity observed three seconds late.
    pub fn applied_ratio_lu(&self) -> f64 {
        self.control_ratio_lu + self.applied_gain_reduction_db
    }

    /// The ratio target currently being aimed at (normal or over-voice), for the GUI.
    pub fn active_target_ratio_lu(&self) -> f64 {
        self.automix_engine.active_target_ratio_lu()
    }

    /// A display ratio consistent with the two momentary bars a GUI draws either side of it:
    /// Dialogue momentary against Bed's *effective* (post-reduction) momentary. Floored at 0 - a
    /// negative "distance" is not a meaningful thing to show, and COM dipping below the Bed during
    /// natural gaps is normal rather than alarming.
    pub fn display_ratio_lu(&self) -> f64 {
        (self.dialogue_meter.momentary_loudness_db() - self.bed_effective_momentary_lufs()).max(0.0)
    }

    /// Bed (IS) momentary loudness, dry - the incoming programme level. With feed-forward this is
    /// the same meter the control loop uses, so the displayed IS level and the controlled one no
    /// longer disagree (they were separate meters under the old closed-loop design).
    pub fn bed_momentary_lufs(&self) -> f64 {
        self.bed_meter.momentary_loudness_db()
    }

    /// What Bed actually sounds like right now: the dry momentary reading minus the applied
    /// reduction. Exact for a constant gain and a close approximation while it moves.
    pub fn bed_effective_momentary_lufs(&self) -> f64 {
        self.bed_meter.momentary_loudness_db() - self.applied_gain_reduction_db
    }

    /// Dialogue (COM) momentary loudness. Never gained, so there is no pre/post distinction.
    pub fn dialogue_momentary_lufs(&self) -> f64 {
        self.dialogue_meter.momentary_loudness_db()
    }
}

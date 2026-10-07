use ebur128::Error as Ebur128Error;

use crate::automix::{
    apply_ramped_gain, AutomixEngine, AutomixEngineConfig, FastLoudnessMeter, LoopContributions,
    LoudnessSnapshot, MixState,
};
use crate::loudness::{ConfigError, Ebur128Meter};

/// The fast stage is stepped once per this many frames rather than once per audio block. At 48kHz
/// that is ~1.3ms, so a 5ms attack is actually realisable; stepping once per block would quantise
/// it to the host's buffer size (often 10-20ms) and the stage would lose the transient response
/// that justifies its existence.
const FAST_SUBCHUNK_FRAMES: usize = 64;

/// Integration time of the fast detectors. Well below R128 momentary (400ms), which is the point,
/// but not so short that it reads individual syllables as level changes - 30ms was tried first and
/// made the stage track the speech envelope rather than its onsets.
const FAST_WINDOW_SECONDS: f64 = 0.05;

/// Bundles the loudness detectors, the multi-loop gain computer and the output delay that every
/// wrapper (GStreamer element, VST3 plugin, `com-is-assist-offline`) needs byte-for-byte
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
/// own schedules (a GStreamer element's two sink pads, each on its own streaming thread).
pub struct AutomixProcessor {
    bed_channels: u32,
    sample_rate: u32,

    /// Dry Bed and Dialogue at R128 windows. One instance per signal serves *both* the slow (3s
    /// short-term) and mid (400ms momentary) stages, since a single meter exposes both.
    bed_meter: Ebur128Meter,
    dialogue_meter: Ebur128Meter,
    /// Dry Bed at ~50ms, for the fast stage. There is deliberately no Dialogue equivalent - see
    /// [`LoudnessSnapshot`] for why only the Bed is read fast.
    bed_fast: FastLoudnessMeter,
    /// COM short-term, cached at each control step so the fast stage can use it between ticks
    /// without re-querying the meter per sub-chunk.
    dialogue_reference_lufs: f64,

    automix_engine: AutomixEngine,

    /// The gain actually applied to the most recent sample, so the next chunk can ramp from it
    /// rather than stepping. Replaces the old tick-length ramp: with the fast stage updating every
    /// sub-chunk there is no fixed ramp window any more.
    current_gain: f32,
    automix_enabled: bool,

    /// Frames fed into each side since the last control step. A new slow/mid step only runs once
    /// both have accumulated a full tick - this is the one place the ~100ms control cadence lives.
    bed_frames_since_control: u64,
    dialogue_frames_since_control: u64,
    tick_frames: u64,

    applied_gain_reduction_db: f64,
    control_ratio_lu: f64,
    mix_state: MixState,
}

impl AutomixProcessor {
    pub fn new(
        bed_channels: u32,
        sample_rate: u32,
        automix_config: AutomixEngineConfig,
        tick_seconds: f64,
    ) -> Result<Self, ConfigError> {
        let bed_map = Ebur128Meter::bed_channel_map(bed_channels)?;
        Ok(Self {
            bed_channels,
            sample_rate,
            bed_meter: Ebur128Meter::new(&bed_map, sample_rate)?,
            dialogue_meter: Ebur128Meter::new(&Ebur128Meter::dialogue_channel_map(), sample_rate)?,
            bed_fast: FastLoudnessMeter::new(&bed_map, sample_rate, FAST_WINDOW_SECONDS),
            dialogue_reference_lufs: Ebur128Meter::NEGATIVE_INFINITY_DB,
            automix_engine: AutomixEngine::new(automix_config),
            current_gain: 1.0,
            automix_enabled: true,
            bed_frames_since_control: 0,
            dialogue_frames_since_control: 0,
            tick_frames: (sample_rate as f64 * tick_seconds) as u64,
            applied_gain_reduction_db: 0.0,
            control_ratio_lu: 0.0,
            mix_state: MixState::ReleaseToUnity,
        })
    }

    pub fn bed_channels(&self) -> u32 {
        self.bed_channels
    }

    /// Refreshes the live-adjustable config without resetting any accumulated state. Safe to call
    /// every block - it is a couple of struct copies, not a rebuild.
    pub fn set_config(&mut self, automix_config: AutomixEngineConfig) {
        self.automix_engine.set_config(automix_config);
    }

    /// `false` bypasses automix: Bed passes at unity gain. Metering keeps running.
    pub fn set_automix_enabled(&mut self, enabled: bool) {
        self.automix_enabled = enabled;
    }

    /// Feeds one chunk of *dry* Dialogue into the Dialogue meter and advances the control cadence.
    /// Dialogue is never gained and never delayed, so there is no equivalent of `process_bed` on
    /// this side - the caller forwards its own buffer untouched.
    pub fn feed_dialogue(&mut self, dry_dialogue: &[f32]) -> Result<(), Ebur128Error> {
        self.dialogue_meter.push_frames(dry_dialogue)?;
        self.dialogue_frames_since_control += dry_dialogue.len() as u64;
        Ok(())
    }

    /// The Bed signal path, in place: measures the dry signal, then steps the fast stage and
    /// applies the resulting gain in sub-chunks.
    ///
    /// Order matters and is the whole feed-forward idea: audio is *measured* before any gain is
    /// applied to it, so the control value that reaches a given sample was derived from that same
    /// sample rather than from one three seconds old. Nothing is buffered or held back, so this
    /// path adds exactly zero latency - a property the GStreamer deployment depends on (see
    /// `Specs/TechnicalConcept.md` section 7).
    pub fn process_bed(&mut self, bed: &mut [f32]) -> Result<(), Ebur128Error> {
        let channels = self.bed_channels as usize;
        if channels == 0 || bed.is_empty() {
            return Ok(());
        }

        // Dry measurement, before anything is applied.
        self.bed_meter.push_frames(bed)?;
        self.bed_frames_since_control += bed.len() as u64 / channels as u64;

        let subchunk_samples = FAST_SUBCHUNK_FRAMES * channels;

        for subchunk in bed.chunks_mut(subchunk_samples) {
            self.bed_fast.push_frames(subchunk);

            // The final sub-chunk of a block is usually short, so derive dt from its real length
            // rather than assuming a full one - the fast stage's ballistics are in seconds.
            let dt = (subchunk.len() / channels) as f64 / self.sample_rate as f64;
            let reduction_db =
                self.automix_engine
                    .process_fast(self.bed_fast.loudness_db(), self.dialogue_reference_lufs, dt);
            self.applied_gain_reduction_db = reduction_db;

            let target_gain = if self.automix_enabled {
                10f64.powf(-reduction_db / 20.0) as f32
            } else {
                1.0
            };

            apply_ramped_gain(subchunk, self.bed_channels, self.current_gain, target_gain);
            self.current_gain = target_gain;
        }

        Ok(())
    }

    /// Recomputes the slow and mid stages once enough new audio has been fed into both sides since
    /// the last control step. A no-op otherwise, so it is safe to call after every chunk from
    /// either side.
    ///
    /// `com_voice_active`/`is_voice_active` come from the caller's two `SileroVad` instances (see
    /// `crate::voice_activity`); together they select the [`MixState`] that drives this tick.
    pub fn maybe_run_control_step(&mut self, com_voice_active: bool, is_voice_active: bool) {
        if self.bed_frames_since_control < self.tick_frames
            || self.dialogue_frames_since_control < self.tick_frames
        {
            return;
        }
        let dt = self.bed_frames_since_control as f64 / self.sample_rate as f64;
        self.bed_frames_since_control = 0;
        self.dialogue_frames_since_control = 0;

        let loudness = LoudnessSnapshot {
            bed_short_term_lufs: self.bed_meter.short_term_loudness_db(),
            bed_momentary_lufs: self.bed_meter.momentary_loudness_db(),
            dialogue_short_term_lufs: self.dialogue_meter.short_term_loudness_db(),
        };
        self.dialogue_reference_lufs = loudness.dialogue_short_term_lufs;
        self.control_ratio_lu = loudness.dialogue_short_term_lufs - loudness.bed_short_term_lufs;

        let result = self
            .automix_engine
            .process_tick(loudness, com_voice_active, is_voice_active, dt);
        self.applied_gain_reduction_db = result.gain_reduction_db;
        self.mix_state = result.state;
    }

    // ---- metering ----------------------------------------------------------------------------

    pub fn applied_gain_reduction_db(&self) -> f64 {
        self.applied_gain_reduction_db
    }

    /// What each stage of the cascade is contributing right now - the fastest answer to "why is it
    /// doing that?", since the three stages have visibly different characters.
    pub fn contributions(&self) -> LoopContributions {
        self.automix_engine.contributions()
    }

    pub fn mix_state(&self) -> MixState {
        self.mix_state
    }

    /// The COM/IS ratio the control loop is actually achieving: the dry ratio plus whatever
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

use ebur128::Error as Ebur128Error;

use crate::automix::{apply_ramped_gain_at, ramp_value_at, AutomixEngine, AutomixEngineConfig, GainComputerConfig};
use crate::loudness::{ConfigError, Ebur128Meter};
use crate::ratio::RatioEngine;

/// Bundles the loudness meters, ratio/gain engines, and continuous gain ramp that every wrapper
/// (GStreamer element, VST3 plugin, `com-is-assist-offline`) needs byte-for-byte identically. This is what
/// actually enforces "no DSP divergence between wrappers" (see `Specs/TechnicalConcept.md` section
/// 8): there is exactly one implementation of "how audio gets gained and how the ratio/gain target
/// gets recomputed," and every wrapper calls into it rather than re-deriving its own copy.
///
/// Deliberately decoupled from how audio is chunked or how often each side is fed: Bed and
/// Dialogue can arrive together every call (a VST3 host's synchronous `process()`) or
/// independently on their own schedules (a GStreamer element's two sink pads, each on its own
/// streaming thread) - `feed_bed`/`feed_dialogue` each track their own frame count since the last
/// control step, and a new ratio/gain target is only computed once *both* have accumulated a full
/// tick's worth (see `maybe_run_control_step`).
pub struct AutomixProcessor {
    bed_channels: u32,
    bed_meter: Ebur128Meter,
    /// A second Bed meter, fed the *dry* (pre-gain) signal, purely for display (`Specs/UI.md`'s
    /// "IS LUFS-M") - kept entirely separate from `bed_meter`, which must keep observing the
    /// already-gained signal for the closed control loop (`Specs/TechnicalConcept.md` section 4)
    /// to behave correctly. Showing the automix engine's own already-gained view of Bed would be
    /// misleading as an "IS loudness" readout, since it'd already reflect this processor's own
    /// gain reduction rather than the incoming program level.
    bed_pre_gain_meter: Ebur128Meter,
    dialogue_meter: Ebur128Meter,
    ratio_engine: RatioEngine,
    automix_engine: AutomixEngine,

    /// The gain ramp is continuous, decoupled from both control cadence and how callers chunk
    /// their audio: `ramp_start_gain` is where it began, `ramp_target_gain` is where the most
    /// recent control step decided it should end up, and `ramp_position_frames`/`ramp_total_frames`
    /// track how far through that ramp the most recently processed Bed frame is. Each call to
    /// `apply_gain_to_bed_chunk` (with whatever chunk size the caller has) advances
    /// `ramp_position_frames` by tha1t chunk's frame count - see `apply_ramped_gain_at`'s doc
    /// comment for why this reproduces the same smooth ramp a whole-tick batch would.
    ramp_start_gain: f32,
    ramp_target_gain: f32,
    ramp_position_frames: u64,
    ramp_total_frames: u64,

    /// Frames fed into each meter since the last control step. A new ratio/gain target is only
    /// computed once both reach `tick_frames` - this is the one place the ~100ms control cadence
    /// lives; it has nothing to do with how callers chunk or forward their audio.
    bed_frames_since_control: u64,
    dialogue_frames_since_control: u64,
    tick_frames: u64,

    applied_gain_reduction_db: f64,
    applied_ratio_lu: f64,
}

impl AutomixProcessor {
    pub fn new(
        bed_channels: u32,
        sample_rate: u32,
        automix_config: AutomixEngineConfig,
        gain_config: GainComputerConfig,
        tick_seconds: f64,
    ) -> Result<Self, ConfigError> {
        let tick_frames = (sample_rate as f64 * tick_seconds) as u64;
        Ok(Self {
            bed_channels,
            bed_meter: Ebur128Meter::new(&Ebur128Meter::bed_channel_map(bed_channels)?, sample_rate)?,
            bed_pre_gain_meter: Ebur128Meter::new(&Ebur128Meter::bed_channel_map(bed_channels)?, sample_rate)?,
            dialogue_meter: Ebur128Meter::new(&Ebur128Meter::dialogue_channel_map(), sample_rate)?,
            ratio_engine: RatioEngine::default(),
            automix_engine: AutomixEngine::new(automix_config, gain_config, tick_seconds),
            ramp_start_gain: 1.0,
            ramp_target_gain: 1.0,
            ramp_position_frames: 0,
            ramp_total_frames: tick_frames,
            bed_frames_since_control: 0,
            dialogue_frames_since_control: 0,
            tick_frames,
            applied_gain_reduction_db: 0.0,
            applied_ratio_lu: 0.0,
        })
    }

    pub fn bed_channels(&self) -> u32 {
        self.bed_channels
    }

    pub fn applied_gain_reduction_db(&self) -> f64 {
        self.applied_gain_reduction_db
    }

    /// The last computed COM/IS ratio, in LU (signed - can be negative, e.g. when COM briefly
    /// falls quieter than the already-gained Bed). This is the actual control-loop value: what
    /// `maybe_run_control_step` feeds `AutomixEngine` to decide shortfall/excess. For a
    /// display-only ratio consistent with `bed_momentary_lufs`/`dialogue_momentary_lufs`, see
    /// `display_ratio_lu` instead - don't use this one to draw a bar between those two, since it's
    /// computed from different meters/time-constants entirely (see that method's doc comment).
    /// `0.0` until the first control step runs.
    pub fn applied_ratio_lu(&self) -> f64 {
        self.applied_ratio_lu
    }

    /// A display-only COM/IS ratio, matching what's actually audible rather than either the raw
    /// incoming Bed level or the control loop's own (short-term) internals: `dialogue_meter`'s
    /// momentary loudness against `bed_effective_momentary_lufs` (pre-gain Bed momentary, minus
    /// the currently-applied reduction - see that method's doc comment for why this approximates
    /// "what you actually hear" from Bed right now). Deliberately not `applied_ratio_lu` (the
    /// short-term, post-gain value that actually drives automix decisions) - that's computed from
    /// different meters/time-constants entirely, so a GUI drawing a bar between its displayed IS
    /// and COM bars needs this one to stay visually consistent with both. Floored at `0.0`: a
    /// negative "distance" isn't a meaningful quantity to show here (COM having brief natural gaps
    /// below Bed's effective level is normal, not itself alarming) - `applied_ratio_lu` is what
    /// still carries the signed shortfall/excess information the actual control loop needs.
    pub fn display_ratio_lu(&self) -> f64 {
        (self.dialogue_meter.momentary_loudness_db() - self.bed_effective_momentary_lufs()).max(0.0)
    }

    /// Bed (IS), *pre-gain*, momentary R128 loudness - the incoming program level, not affected by
    /// this processor's own gain reduction (see `bed_pre_gain_meter`'s doc comment). For display
    /// only (`Specs/UI.md`'s "IS LUFS-M"); not used by the automix control loop itself, which
    /// works from `bed_meter`'s (post-gain) *short-term* loudness (see `maybe_run_control_step`).
    pub fn bed_momentary_lufs(&self) -> f64 {
        self.bed_pre_gain_meter.momentary_loudness_db()
    }

    /// An approximation of Bed's *audible* (post-gain) momentary loudness: `bed_momentary_lufs`
    /// minus `applied_gain_reduction_db`. Not a genuinely separate measurement - `bed_meter` (the
    /// real post-gain meter, used for control) only tracks *short-term* loudness, not momentary
    /// (see `maybe_run_control_step`), and adding a third full `Ebur128Meter` just for a display
    /// value wasn't worth it when gain reduction is applied uniformly across the whole chunk: a
    /// linear dB shift of the pre-gain momentary reading is exact for a constant gain, and a very
    /// close approximation given how slowly the gain ramp moves relative to the momentary window.
    /// For display only, alongside `display_ratio_lu` (which uses this as Bed's side of the gap,
    /// not the raw pre-gain reading, so the shown ratio reflects what's actually audible).
    pub fn bed_effective_momentary_lufs(&self) -> f64 {
        self.bed_pre_gain_meter.momentary_loudness_db() - self.applied_gain_reduction_db
    }

    /// Dialogue (COM) momentary R128 loudness, for display only - see `bed_momentary_lufs`'s doc
    /// comment (Dialogue is never gained, so there's no pre/post distinction on this side).
    pub fn dialogue_momentary_lufs(&self) -> f64 {
        self.dialogue_meter.momentary_loudness_db()
    }

    /// Refreshes the live-adjustable config (target ratio/tolerances/gain-reduction ceiling,
    /// attack/hold/release) without resetting any accumulated state - see
    /// `AutomixEngine::set_config`'s doc comment for why this matters (a wrapper that only reads
    /// its parameters once at construction time would otherwise never see later changes take
    /// effect). Safe to call every tick/every `process()` call - it's just a couple of struct
    /// copies, not a rebuild.
    pub fn set_config(&mut self, automix_config: AutomixEngineConfig, gain_config: GainComputerConfig) {
        self.automix_engine.set_config(automix_config, gain_config);
    }

    fn current_ramp_gain(&self) -> f32 {
        ramp_value_at(self.ramp_start_gain, self.ramp_target_gain, self.ramp_position_frames, self.ramp_total_frames)
    }

    /// Feeds one chunk of *dry* (pre-gain) Bed audio into the display-only pre-gain meter (see
    /// `bed_pre_gain_meter`'s doc comment) - call this before `apply_gain_to_bed_chunk`, which
    /// mutates its argument in place, so the dry signal is no longer available afterward. Doesn't
    /// touch the control-cadence counters - `feed_bed` (the post-gain, control-loop-facing meter)
    /// is what those track.
    pub fn feed_bed_pre_gain(&mut self, dry_bed: &[f32]) -> Result<(), Ebur128Error> {
        self.bed_pre_gain_meter.push_frames(dry_bed)
    }

    /// Applies the continuous gain ramp to one chunk of interleaved Bed audio (any size) and
    /// advances the ramp position by that chunk's frame count. Does not feed the loudness meter or
    /// advance the control cadence on its own - pass the result to `feed_bed` for that, so callers
    /// that need to inspect or forward the gained audio before measuring it still can.
    pub fn apply_gain_to_bed_chunk(&mut self, bed: &mut [f32]) {
        let frame_count = bed.len() as u64 / self.bed_channels as u64;
        apply_ramped_gain_at(
            bed,
            self.bed_channels,
            self.ramp_start_gain,
            self.ramp_target_gain,
            self.ramp_position_frames,
            self.ramp_total_frames,
        );
        self.ramp_position_frames += frame_count;
    }

    /// Feeds one chunk of already-gained Bed audio into the loudness meter (closed loop - see
    /// `Specs/TechnicalConcept.md` section 4: the Bed meter must observe the already-gained
    /// signal, not the dry one) and advances the control-cadence counter.
    pub fn feed_bed(&mut self, gained_bed: &[f32]) -> Result<(), Ebur128Error> {
        self.bed_meter.push_frames(gained_bed)?;
        self.bed_frames_since_control += gained_bed.len() as u64 / self.bed_channels as u64;
        Ok(())
    }

    /// Feeds one chunk of dry Dialogue audio into the loudness meter and advances the
    /// control-cadence counter. Dialogue is never gained, so there's no equivalent to
    /// `apply_gain_to_bed_chunk` on this side.
    pub fn feed_dialogue(&mut self, dialogue: &[f32]) -> Result<(), Ebur128Error> {
        self.dialogue_meter.push_frames(dialogue)?;
        self.dialogue_frames_since_control += dialogue.len() as u64;
        Ok(())
    }

    /// Recomputes the ratio/gain target once enough new audio has been fed into both meters since
    /// the last control step (see the `bed_frames_since_control`/`dialogue_frames_since_control`
    /// doc comment), and starts a fresh ramp from the current instantaneous gain toward it. A
    /// no-op otherwise, so it's safe to call after every single chunk fed from either side.
    pub fn maybe_run_control_step(&mut self, automix_enabled: bool) {
        if self.bed_frames_since_control < self.tick_frames || self.dialogue_frames_since_control < self.tick_frames {
            return;
        }
        self.bed_frames_since_control = 0;
        self.dialogue_frames_since_control = 0;

        let bed_s = self.bed_meter.short_term_loudness_db();
        let com_s = self.dialogue_meter.short_term_loudness_db();
        let ratio = self.ratio_engine.update(bed_s, com_s);
        self.applied_ratio_lu = ratio.ratio_lu;
        let automix_result = self.automix_engine.process_tick(ratio);
        self.applied_gain_reduction_db = automix_result.gain_reduction_db;

        self.ramp_start_gain = self.current_ramp_gain();
        self.ramp_target_gain = if automix_enabled { automix_result.gain_linear as f32 } else { 1.0 };
        self.ramp_position_frames = 0;
        self.ramp_total_frames = self.tick_frames;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processor(bed_channels: u32) -> AutomixProcessor {
        AutomixProcessor::new(
            bed_channels,
            48_000,
            AutomixEngineConfig::default(),
            GainComputerConfig::default(),
            0.1,
        )
        .expect("valid processor config")
    }

    /// A real oscillating tone, not a constant value - K-weighting's high-pass corner attenuates
    /// near-DC content so heavily that a constant-valued buffer (e.g. `vec![0.9; N]`) reads as
    /// very quiet regardless of its amplitude, which would make loudness-based test assertions
    /// meaningless.
    fn sine_tone_mono(frequency_hz: f64, amplitude: f32, sample_rate: u32, frame_count: usize) -> Vec<f32> {
        (0..frame_count)
            .map(|i| amplitude * (2.0 * std::f64::consts::PI * frequency_hz * i as f64 / sample_rate as f64).sin() as f32)
            .collect()
    }

    fn interleave_stereo(mono: &[f32]) -> Vec<f32> {
        mono.iter().flat_map(|&s| [s, s]).collect()
    }

    #[test]
    fn starts_at_unity_gain_with_no_reduction() {
        let mut p = processor(2);
        let mut bed = vec![0.5_f32; 200];
        p.apply_gain_to_bed_chunk(&mut bed);
        assert_eq!(bed, vec![0.5_f32; 200]);
        assert_eq!(p.applied_gain_reduction_db(), 0.0);
    }

    #[test]
    fn control_step_is_a_no_op_until_both_sides_reach_a_full_tick() {
        let mut p = processor(1);
        // 100ms @ 48kHz = 4800 frames. Feed less than that on each side.
        p.feed_bed(&vec![0.9_f32; 1000]).unwrap();
        p.feed_dialogue(&vec![0.01_f32; 1000]).unwrap();
        let gain_before = p.current_ramp_gain();
        p.maybe_run_control_step(true);
        assert_eq!(p.current_ramp_gain(), gain_before, "target shouldn't move before a full tick accumulates");
    }

    #[test]
    fn display_ratio_lu_is_floored_at_zero_when_com_is_silent_but_bed_is_loud() {
        let mut p = processor(2);
        let loud_bed_mono = sine_tone_mono(400.0, 0.9, 48_000, 48_000); // 1s, well above the momentary window
        p.feed_bed_pre_gain(&interleave_stereo(&loud_bed_mono)).unwrap();
        // Real (not just never-fed) silence - matches `Ebur128Meter`'s own silence-sentinel test.
        p.feed_dialogue(&vec![0.0_f32; 48_000]).unwrap();
        // Without the floor, this would be a large negative number (dialogue's -inf sentinel
        // minus bed's real, loud reading) rather than the "no measurable lead" 0.0 it should show.
        assert_eq!(p.display_ratio_lu(), 0.0);
    }

    #[test]
    fn display_ratio_lu_reflects_the_gap_when_dialogue_is_louder_than_bed() {
        let mut p = processor(2);
        let quiet_bed_mono = sine_tone_mono(400.0, 0.01, 48_000, 48_000);
        let loud_dialogue = sine_tone_mono(400.0, 0.9, 48_000, 48_000);
        p.feed_bed_pre_gain(&interleave_stereo(&quiet_bed_mono)).unwrap();
        p.feed_dialogue(&loud_dialogue).unwrap();
        assert!(p.display_ratio_lu() > 0.0, "dialogue louder than bed should give a positive display ratio");
    }

    #[test]
    fn bed_effective_momentary_lufs_matches_pre_gain_when_nothing_has_been_reduced_yet() {
        let mut p = processor(2);
        let bed_mono = sine_tone_mono(400.0, 0.5, 48_000, 48_000);
        p.feed_bed_pre_gain(&interleave_stereo(&bed_mono)).unwrap();
        assert_eq!(p.bed_effective_momentary_lufs(), p.bed_momentary_lufs());
    }

    #[test]
    fn bed_effective_momentary_lufs_subtracts_the_currently_applied_reduction() {
        let mut p = processor(1);
        // Build up some real reduction via the actual control loop first (see
        // `set_config_does_not_reset_already_accumulated_gain_reduction` for why these
        // levels/tick counts).
        let loud_bed = sine_tone_mono(400.0, 0.9, 48_000, 4_800);
        let quiet_dialogue = sine_tone_mono(400.0, 0.05, 48_000, 4_800);
        for _ in 0..80 {
            p.feed_bed(&loud_bed).unwrap();
            p.feed_dialogue(&quiet_dialogue).unwrap();
            p.maybe_run_control_step(true);
        }
        let reduction = p.applied_gain_reduction_db();
        assert!(reduction > 0.0, "expected some reduction to have accumulated by now, got {reduction}dB");

        // Feed the same loud tone into the *pre-gain* meter too (a real wrapper feeds both every
        // block - see e.g. the VST3 `process()` call site) so `bed_momentary_lufs` has a real
        // reading to subtract the reduction from.
        p.feed_bed_pre_gain(&loud_bed).unwrap();

        let expected = p.bed_momentary_lufs() - reduction;
        assert!(
            (p.bed_effective_momentary_lufs() - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            p.bed_effective_momentary_lufs()
        );
    }

    #[test]
    fn set_config_does_not_reset_already_accumulated_gain_reduction() {
        let mut p = processor(1);
        // A loud bed against a quiet-but-present dialogue (above the "currently silent" floor)
        // should build up some real reduction over a number of ticks (4800 frames = one 100ms
        // tick at 48kHz; RatioEngine also needs ~20 consecutive non-silent ticks before it
        // considers the ratio valid, so run comfortably more than that).
        let loud_bed = sine_tone_mono(400.0, 0.9, 48_000, 4_800);
        let quiet_dialogue = sine_tone_mono(400.0, 0.05, 48_000, 4_800);
        for _ in 0..80 {
            p.feed_bed(&loud_bed).unwrap();
            p.feed_dialogue(&quiet_dialogue).unwrap();
            p.maybe_run_control_step(true);
        }
        let before = p.applied_gain_reduction_db();
        assert!(before > 0.0, "expected some reduction to have accumulated by now, got {before}dB");

        p.set_config(AutomixEngineConfig::default(), GainComputerConfig::default());
        assert_eq!(p.applied_gain_reduction_db(), before, "set_config shouldn't reset already-applied reduction");
    }
}

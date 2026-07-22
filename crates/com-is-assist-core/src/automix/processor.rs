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
    /// `ramp_position_frames` by that chunk's frame count - see `apply_ramped_gain_at`'s doc
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

    /// The last computed COM/IS ratio, in LU. `0.0` until the first control step runs.
    pub fn applied_ratio_lu(&self) -> f64 {
        self.applied_ratio_lu
    }

    /// Bed (IS), *pre-gain*, momentary R128 loudness - the incoming program level, not affected by
    /// this processor's own gain reduction (see `bed_pre_gain_meter`'s doc comment). For display
    /// only (`Specs/UI.md`'s "IS LUFS-M"); not used by the automix control loop itself, which
    /// works from `bed_meter`'s (post-gain) *short-term* loudness (see `maybe_run_control_step`).
    pub fn bed_momentary_lufs(&self) -> f64 {
        self.bed_pre_gain_meter.momentary_loudness_db()
    }

    /// Dialogue (COM) momentary R128 loudness, for display only - see `bed_momentary_lufs`'s doc
    /// comment (Dialogue is never gained, so there's no pre/post distinction on this side).
    pub fn dialogue_momentary_lufs(&self) -> f64 {
        self.dialogue_meter.momentary_loudness_db()
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
}

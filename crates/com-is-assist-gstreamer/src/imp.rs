// GstComISAssist: 2 sink pads (bed_sink, dialogue_sink) / 3 src pads (is_leveled_src,
// dialogue_src, mix_src), wired to com_is_assist_core's feed-forward multi-loop automix
// (dry detectors -> LoopBank -> gain). See Specs/TechnicalConcept.md section 7 for the element
// design, section 4 for why the detectors observe the *dry* signal, and 5.2 for the cascade.
//
// Bed negotiates 2-6ch dynamically via caps (48kHz, F32LE - see `ranged_audio_caps`); Dialogue is
// always fixed mono. `ProcessingState` (and therefore the `AutomixProcessor` inside it) is only
// constructed once Bed's Caps event reports its actual channel count - see `handle_bed_caps`.
// Pad synchronization is hand-rolled (per-pad accumulation buffers) rather than GstCollectPads,
// which gstreamer-rs 0.25 doesn't wrap safely.
//
// Output latency is deliberately decoupled from the automix control rate: audio is forwarded to
// is_leveled_src/dialogue_src the moment it arrives (near-zero added latency), and mix_src as soon
// as both sides have anything to pair (bounded only by upstream arrival jitter between the two
// sinks, not by a fixed wait). The ratio/gain *target* is still only recomputed once per
// ~TICK_SECONDS (matching BS.1770's own 100ms gating-block granularity - ticking faster wouldn't
// yield more frequent loudness information anyway), but that target is approached via a continuous
// per-frame ramp rather than a per-tick batch, so nothing has to wait for a whole tick to buffer
// before being forwarded. See `ProcessingState` below.

use com_is_assist_core::automix::{
    bed_lrc_channels, mix_dialogue_into_bed, AutomixEngineConfig, AutomixProcessor, MixState,
};
use com_is_assist_core::voice_activity::{SileroVad, VoiceActivityConfig};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

/// A single tick's DSP failure (or an earlier panic) must never make the whole live session go
/// permanently silent. `std::sync::Mutex` poisons itself on any panic while held, and every
/// subsequent `.lock().unwrap()` would then also panic - recover the inner state instead so one
/// bad tick degrades gracefully rather than cascading into "silent forever" for the rest of the
/// stream (the exact failure mode that made an initial live-audio panic so hard to diagnose).
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

const TICK_SECONDS: f64 = 0.1;
const SAMPLE_RATE: u32 = 48_000;
const MIN_BED_CHANNELS: i32 = 2;
const MAX_BED_CHANNELS: i32 = 6;

/// Fixed caps for a specific channel count - used for `dialogue_sink`/`dialogue_src`, which are
/// always mono.
fn fixed_audio_caps(channels: i32) -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("rate", SAMPLE_RATE as i32)
        .field("channels", channels)
        .build()
}

/// A channel-count *range* - used for `bed_sink`/`is_leveled_src`/`mix_src`'s pad templates, since
/// Bed is 2-6ch and the actual count is only known once caps negotiation completes (see
/// `handle_bed_caps`), not at template-declaration time.
fn ranged_audio_caps(min_channels: i32, max_channels: i32) -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("rate", SAMPLE_RATE as i32)
        .field("channels", &gst::IntRange::<i32>::new(min_channels, max_channels))
        .build()
}

fn bytes_to_f32_vec(data: &[u8]) -> Vec<f32> {
    data.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Builds an output buffer with PTS/duration set from `frame_index`/`frame_count` (at
/// `SAMPLE_RATE`). Every buffer this element creates should carry a real timestamp - required
/// for correct behavior in downstream elements that care about timing (muxers, sync=true sinks,
/// etc.), and part of getting this element's pipeline hang (see the `with_class` comments below)
/// fully resolved alongside the `queue` requirement.
fn f32_slice_to_buffer(samples: &[f32], channels: u32, frame_index: u64) -> gst::Buffer {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    let mut buffer = gst::Buffer::from_slice(bytes);
    let frame_count = samples.len() as u64 / channels as u64;
    let pts = gst::ClockTime::from_nseconds(frame_index * 1_000_000_000 / SAMPLE_RATE as u64);
    let duration = gst::ClockTime::from_nseconds(frame_count * 1_000_000_000 / SAMPLE_RATE as u64);
    if let Some(buffer_mut) = buffer.get_mut() {
        buffer_mut.set_pts(Some(pts));
        buffer_mut.set_duration(Some(duration));
    }
    buffer
}

#[derive(Debug, Clone, Copy)]
struct Settings {
    target_ratio_lu: f64,
    /// The higher absolute ratio required while both COM and IS carry voice (the "over-voice"
    /// double-talk case) - see `com_is_assist_core::automix::MixState`.
    overvoice_ratio_lu: f64,
    max_gain_reduction_db: f64,
    /// The single timing control - scales every loop stage's ballistics together. Replaces the
    /// former attack/hold/release/adaptation/interview-release set, which the multi-loop cascade
    /// made meaningless (its effective ballistics are emergent).
    speed: f64,
    /// Lookahead in milliseconds, default 0. Non-zero delays both outputs and is added to the
    /// element's reported latency.
    lookahead_ms: f64,
    interview_passthrough_enabled: bool,
    automix_enabled: bool,
    /// "Voice divergence" (`Specs/UI.md`), 0.0-100.0 (a percentage, matching the VST3 wrapper's
    /// convention): 0% = Dialogue is Center-only in `mix_src` when Bed has a Center channel
    /// (3/5/6ch - see `com_is_assist_core::automix::bed_lrc_channels`), 100% = split across
    /// Left/Right only. Has no audible effect for 2ch/4ch Bed, which has no Center channel to
    /// diverge from - see `mix_dialogue_into_bed`'s doc comment.
    divergence_percent: f64,
}

impl Default for Settings {
    fn default() -> Self {
        let automix = AutomixEngineConfig::default();
        Self {
            target_ratio_lu: automix.target_ratio_lu,
            overvoice_ratio_lu: automix.overvoice_ratio_lu,
            max_gain_reduction_db: automix.max_gain_reduction_db,
            speed: automix.speed,
            lookahead_ms: 0.0,
            interview_passthrough_enabled: automix.interview_passthrough_enabled,
            automix_enabled: true,
            divergence_percent: 0.0,
        }
    }
}

impl Settings {
    fn automix_config(&self) -> AutomixEngineConfig {
        AutomixEngineConfig {
            target_ratio_lu: self.target_ratio_lu,
            overvoice_ratio_lu: self.overvoice_ratio_lu,
            max_gain_reduction_db: self.max_gain_reduction_db,
            speed: self.speed,
            interview_passthrough_enabled: self.interview_passthrough_enabled,
        }
    }
}

/// All state a single incoming audio chunk (from either sink) needs to touch: the shared
/// `AutomixProcessor` (loudness meters, ratio/gain engines, continuous gain ramp - identical to
/// what the VST3 plugin uses, see `com_is_assist_core::automix::AutomixProcessor`'s doc comment) and the
/// small cross-pad buffers used only to align Bed and Dialogue for `mix_src`. Bundled into one
/// struct behind one lock because every one of these is read or written on essentially every
/// chunk from either side - see the `ComISAssist::state` field doc comment for why a single lock
/// (not one per field) is required.
struct ProcessingState {
    processor: AutomixProcessor,

    /// Copied from `Settings::divergence_percent` (as a `[0.0, 1.0]` fraction) at construction
    /// time - see that field's doc comment. Refreshed whenever `ProcessingState` is rebuilt on a
    /// settings change, same tradeoff already accepted for the other settings (see `set_property`).
    divergence: f32,

    /// Bed (already gained) and Dialogue samples waiting to be paired for `mix_src`. Drained
    /// immediately whenever *both* have at least one frame available (see `drain_mix_ready`) -
    /// there's no fixed-size wait here, so this only ever holds however much the two sinks'
    /// arrival timing happens to be out of step by, not a designed buffering delay.
    mix_bed_pending: Vec<f32>,
    mix_dialogue_pending: Vec<f32>,
}

impl ProcessingState {
    /// `bed_channels` comes from Bed's negotiated caps (see `handle_bed_caps`), not from
    /// `settings` - it's a property of the stream, not something a user configures.
    fn new(settings: &Settings, bed_channels: u32) -> Result<Self, com_is_assist_core::loudness::ConfigError> {
        Ok(Self {
            processor: {
                let mut processor =
                    AutomixProcessor::new(bed_channels, SAMPLE_RATE, settings.automix_config(), TICK_SECONDS)?;
                processor.set_lookahead_seconds(settings.lookahead_ms / 1000.0);
                processor.set_automix_enabled(settings.automix_enabled);
                processor
            },
            divergence: (settings.divergence_percent / 100.0) as f32,
            mix_bed_pending: Vec::new(),
            mix_dialogue_pending: Vec::new(),
        })
    }
}

/// Drains whatever Bed/Dialogue audio *can* currently be paired for `mix_src` - i.e. up to
/// `min(bed_pending, dialogue_pending)` frames - immediately, with no fixed-size wait. Returns
/// `None` if either side is currently empty; that's normal (just means one side is momentarily
/// ahead) and does not mean the stream has ended. Dialogue is mixed in restricted to Left/Center/
/// Right only, via the shared `mix_dialogue_into_bed` (same function and pan law the VST3 wrapper
/// uses - see its doc comment); Bed channels beyond that (LFE, surrounds) are untouched.
fn drain_mix_ready(state: &mut ProcessingState) -> Option<Vec<f32>> {
    let bed_channels = state.processor.bed_channels();
    let bed_frames = state.mix_bed_pending.len() / bed_channels as usize;
    let dialogue_frames = state.mix_dialogue_pending.len();
    let frames = bed_frames.min(dialogue_frames);
    if frames == 0 {
        return None;
    }
    let mut mix: Vec<f32> = state.mix_bed_pending.drain(0..frames * bed_channels as usize).collect();
    let dialogue_chunk: Vec<f32> = state.mix_dialogue_pending.drain(0..frames).collect();
    let (left, right, center) = bed_lrc_channels(bed_channels);
    mix_dialogue_into_bed(&mut mix, &dialogue_chunk, bed_channels, left, right, center, state.divergence);
    Some(mix)
}

/// Called only once *both* sinks have reached EOS: flushes whatever's left on the longer side,
/// treating the shorter (already-finished) side as silent for that remaining tail. This is the
/// only place "mix alone" is correct - `drain_mix_ready` already continuously drains anything that
/// *can* be paired as it arrives, so by the time both sides are done, any leftover on one side
/// means the other side's stream was simply shorter, not that pairing was skipped.
fn flush_remaining_mix_tail(state: &mut ProcessingState) -> Option<Vec<f32>> {
    let bed_channels = state.processor.bed_channels();
    let bed_frames = state.mix_bed_pending.len() / bed_channels as usize;
    if bed_frames > 0 {
        let bed_chunk: Vec<f32> = state.mix_bed_pending.drain(..).collect();
        state.mix_dialogue_pending.clear();
        return Some(bed_chunk);
    }
    let dialogue_frames = state.mix_dialogue_pending.len();
    if dialogue_frames > 0 {
        let dialogue_chunk: Vec<f32> = state.mix_dialogue_pending.drain(..).collect();
        state.mix_bed_pending.clear();
        let mut mix = vec![0.0f32; dialogue_chunk.len() * bed_channels as usize];
        let (left, right, center) = bed_lrc_channels(bed_channels);
        mix_dialogue_into_bed(&mut mix, &dialogue_chunk, bed_channels, left, right, center, state.divergence);
        return Some(mix);
    }
    None
}

#[cfg(test)]
mod mix_alignment_tests {
    use super::*;

    const EQUAL_POWER: f32 = std::f32::consts::FRAC_1_SQRT_2;

    fn test_state(bed_channels: u32) -> ProcessingState {
        ProcessingState::new(&Settings::default(), bed_channels).expect("valid test config")
    }

    fn assert_close(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-5, "expected {expected:?}, got {actual:?}");
        }
    }

    #[test]
    fn pairs_equal_length_bed_and_dialogue_at_equal_power_no_center_channel() {
        // 2ch (stereo) Bed has no Center channel - see bed_lrc_channels - so Dialogue always
        // lands on both Left/Right at equal power, regardless of divergence (default 0%).
        let mut state = test_state(2);
        state.mix_bed_pending = vec![0.1, 0.2, 0.3, 0.4]; // 2 stereo frames
        state.mix_dialogue_pending = vec![1.0, 2.0]; // 2 mono frames

        let mix = drain_mix_ready(&mut state).expect("both sides have data");
        assert_close(
            &mix,
            &[
                0.1 + EQUAL_POWER,
                0.2 + EQUAL_POWER,
                0.3 + 2.0 * EQUAL_POWER,
                0.4 + 2.0 * EQUAL_POWER,
            ],
        );
        assert!(state.mix_bed_pending.is_empty());
        assert!(state.mix_dialogue_pending.is_empty());
    }

    #[test]
    fn zero_divergence_with_a_center_channel_puts_dialogue_on_center_only() {
        // 3ch Bed (Left, Right, Center) - default divergence is 0%, so Dialogue should land
        // entirely on the Center channel (index 2), not Left/Right.
        let mut state = test_state(3);
        state.mix_bed_pending = vec![0.1, 0.2, 0.3]; // 1 frame, 3 channels
        state.mix_dialogue_pending = vec![1.0];

        let mix = drain_mix_ready(&mut state).expect("both sides have data");
        assert_close(&mix, &[0.1, 0.2, 0.3 + 1.0]);
    }

    #[test]
    fn returns_none_when_either_side_is_empty() {
        let mut state = test_state(2);
        state.mix_bed_pending = vec![0.1, 0.2];
        assert!(drain_mix_ready(&mut state).is_none());

        let mut state = test_state(2);
        state.mix_dialogue_pending = vec![1.0];
        assert!(drain_mix_ready(&mut state).is_none());
    }

    #[test]
    fn drains_only_the_minimum_available_leaving_the_rest_pending() {
        let mut state = test_state(2);
        state.mix_bed_pending = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6]; // 3 stereo frames
        state.mix_dialogue_pending = vec![1.0]; // 1 mono frame

        let mix = drain_mix_ready(&mut state).expect("one frame is pairable");
        assert_close(&mix, &[0.1 + EQUAL_POWER, 0.2 + EQUAL_POWER]);
        assert_eq!(state.mix_bed_pending, vec![0.3, 0.4, 0.5, 0.6]);
        assert!(state.mix_dialogue_pending.is_empty());
    }

    #[test]
    fn flush_tail_treats_leftover_bed_as_mix_alone() {
        let mut state = test_state(2);
        state.mix_bed_pending = vec![0.1, 0.2, 0.3, 0.4];
        state.mix_dialogue_pending.clear();

        let mix = flush_remaining_mix_tail(&mut state).expect("bed has a leftover tail");
        assert_eq!(mix, vec![0.1, 0.2, 0.3, 0.4]);
        assert!(state.mix_bed_pending.is_empty());
    }

    #[test]
    fn flush_tail_expands_leftover_dialogue_at_equal_power_no_center_channel() {
        let mut state = test_state(2);
        state.mix_dialogue_pending = vec![1.0, 2.0];

        let mix = flush_remaining_mix_tail(&mut state).expect("dialogue has a leftover tail");
        assert_close(&mix, &[EQUAL_POWER, EQUAL_POWER, 2.0 * EQUAL_POWER, 2.0 * EQUAL_POWER]);
        assert!(state.mix_dialogue_pending.is_empty());
    }

    #[test]
    fn flush_tail_is_none_when_nothing_is_pending() {
        let mut state = test_state(2);
        assert!(flush_remaining_mix_tail(&mut state).is_none());
    }
}

/// Independent output-frame counters for the three src pads. Unlike the control-rate design this
/// replaced, `is_leveled_src`/`dialogue_src`/`mix_src` no longer advance in lockstep - each is
/// forwarded as soon as its own data is ready, so each needs its own running PTS position.
#[derive(Default)]
struct FrameIndices {
    is_leveled: u64,
    dialogue: u64,
    mix: u64,
}

pub struct ComISAssist {
    settings: Mutex<Settings>,
    /// One lock for all per-chunk state (see `ProcessingState`'s doc comment) rather than one per
    /// field: `bed_sink` and `dialogue_sink` each run their chain function on whatever streaming
    /// thread their own upstream branch uses - two genuinely different OS threads that can both
    /// touch this state concurrently. Every incoming chunk needs to read/advance the shared gain
    /// ramp, feed its own meter, update the shared mix-alignment buffers, and possibly trigger a
    /// control step - all of which must stay consistent with each other, so it's one critical
    /// section rather than several independently-locked fields that could interleave.
    ///
    /// `None` until `bed_sink`'s first Caps event tells us the negotiated Bed channel count (see
    /// `handle_bed_caps`) - `AutomixProcessor::new` needs that count up front and Bed's channel
    /// count is no longer a compile-time constant (2-6ch, negotiated per `ranged_audio_caps`).
    state: Mutex<Option<ProcessingState>>,
    /// Raw Dialogue samples received while `state` is still `None` (Bed hasn't sent its Caps event
    /// yet). Dialogue's own dry passthrough to `dialogue_src` never waits on this - only metering
    /// and mix-alignment do - so this just holds the gap until `handle_bed_caps` replays it into
    /// the freshly-constructed `ProcessingState`.
    pending_dialogue_before_state: Mutex<Vec<f32>>,
    /// Set once the corresponding sink has received EOS. Tracked here rather than inside
    /// `ProcessingState` because EOS can legitimately arrive before `state` exists (e.g. Dialogue
    /// EOS during a Bed-caps-still-pending window). `is_leveled_src`/`dialogue_src` each end as
    /// soon as their own sink's EOS arrives, but `mix_src` can only end once *both* are set - see
    /// `flush_remaining_mix_tail`'s doc comment for how any still-unpaired tail is then handled.
    bed_eos: AtomicBool,
    dialogue_eos: AtomicBool,
    /// Real voice-activity detection on Dialogue (COM) - see
    /// `com_is_assist_core::voice_activity`'s doc comment for why this replaced the old LUFS-floor
    /// "is COM currently silent?" stand-in. Independent of `state`/`ProcessingState`: unlike Bed,
    /// Dialogue's sample rate is always the fixed `SAMPLE_RATE` constant, so this doesn't need to
    /// wait on caps negotiation the way `AutomixProcessor` does, and it's fed unconditionally in
    /// `process_dialogue_chunk` even before `state` exists (matching that method's existing
    /// near-zero-latency, no-waiting-on-Bed-caps principle). `None` if construction ever failed
    /// (e.g. the ONNX Runtime binary couldn't be obtained) - callers then fail open (treat COM as
    /// always voice-active, matching the old pre-VAD behavior) rather than silently never ducking.
    voice_activity: Mutex<Option<SileroVad>>,
    /// The second, Bed-side detector (`Specs/UI.md`'s "voice detector IS"), fed a mono downmix of
    /// the *dry* Bed. Together with `voice_activity` it selects the automix state (see
    /// `com_is_assist_core::automix::MixState`). Same fixed `SAMPLE_RATE` as the COM detector, so
    /// it likewise doesn't need to wait on caps negotiation. `None` if construction failed, in
    /// which case callers fail *closed* (IS treated as never voiced), which simply leaves the
    /// ordinary duck/release states in play rather than inventing an interview/over-voice state.
    is_voice_activity: Mutex<Option<SileroVad>>,
    /// Bed's negotiated channel count, mirrored out of `ProcessingState` so the mono downmix for
    /// `is_voice_activity` can be computed *without* holding the `state` lock - the two locks are
    /// deliberately never held together (see `feed_voice_activity`). `0` until Bed's caps arrive.
    bed_channels: AtomicU32,
    frame_indices: Mutex<FrameIndices>,
    bed_sink: gst::Pad,
    dialogue_sink: gst::Pad,
    is_leveled_src: gst::Pad,
    dialogue_src: gst::Pad,
    mix_src: gst::Pad,
}

impl ComISAssist {
    /// Allocates the next PTS-frame position for one of the three independent output counters
    /// (see `FrameIndices`) and advances it by `frame_count`.
    fn allocate_frame_index(&self, pick: impl FnOnce(&mut FrameIndices) -> &mut u64, frame_count: u64) -> u64 {
        let mut indices = lock_recover(&self.frame_indices);
        let counter = pick(&mut indices);
        let current = *counter;
        *counter += frame_count;
        current
    }

    /// Feeds one chunk of Dialogue audio into `voice_activity` and returns its (possibly just
    /// updated) voice-active state. Fails open (`true`) if VAD wasn't constructed - see
    /// `voice_activity`'s doc comment. Always locks `voice_activity` *before* `state` is ever
    /// locked in the same call chain (see `process_bed_chunk`/`process_dialogue_chunk`) - the two
    /// are never held together, so there's no ordering hazard to maintain beyond that.
    fn feed_voice_activity(&self, dialogue: &[f32]) -> bool {
        match lock_recover(&self.voice_activity).as_mut() {
            Some(vad) => {
                let _ = vad.feed(dialogue);
                vad.voice_active()
            }
            None => true,
        }
    }

    /// Reads the current voice-active state without feeding new audio - for call sites (Bed's own
    /// chunk processing) that need the latest reading but have no Dialogue audio of their own to
    /// feed this call. Fails open (`true`) if VAD wasn't constructed, same as `feed_voice_activity`.
    fn voice_active(&self) -> bool {
        lock_recover(&self.voice_activity).as_ref().map_or(true, |vad| vad.voice_active())
    }

    /// Feeds one chunk of *dry* Bed audio into `is_voice_activity`, downmixed to mono (Silero is a
    /// mono model, and "is anyone speaking anywhere on the Bed" is a whole-Bed question), and
    /// returns the resulting state. Fails closed (`false`) if the detector wasn't constructed or
    /// Bed's channel count isn't known yet. Locked and released without ever holding `state`, same
    /// discipline as `feed_voice_activity`.
    fn feed_is_voice_activity(&self, raw_bed: &[f32]) -> bool {
        let channels = self.bed_channels.load(Ordering::Relaxed) as usize;
        if channels == 0 {
            return false;
        }
        let mono: Vec<f32> = raw_bed
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect();
        match lock_recover(&self.is_voice_activity).as_mut() {
            Some(vad) => {
                let _ = vad.feed(&mono);
                vad.voice_active()
            }
            None => false,
        }
    }

    /// Reads the IS-side voice-active state without feeding new audio - for the Dialogue chain,
    /// which has no Bed audio of its own. Fails closed, same as `feed_is_voice_activity`.
    fn is_voice_active(&self) -> bool {
        lock_recover(&self.is_voice_activity).as_ref().is_some_and(|vad| vad.voice_active())
    }

    fn sink_chain(&self, is_bed: bool, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let map = buffer.map_readable().map_err(|_| {
            gst::element_error!(
                self.obj(),
                gst::ResourceError::Read,
                [
                    "failed to map incoming {} buffer readable",
                    if is_bed { "bed" } else { "dialogue" }
                ]
            );
            gst::FlowError::Error
        })?;
        let samples = bytes_to_f32_vec(&map);
        drop(map);

        if is_bed {
            self.process_bed_chunk(&samples)
        } else {
            self.process_dialogue_chunk(&samples)
        }
    }

    /// Constructs (or replaces, if the negotiated channel count changed) `ProcessingState` once
    /// Bed's Caps event tells us its channel count - `AutomixProcessor::new` needs that count up
    /// front, and it's no longer known at pad-template-declaration time (see `ranged_audio_caps`).
    /// Replays any Dialogue buffered in `pending_dialogue_before_state` (samples that arrived
    /// while Bed's caps were still pending) into the freshly-built state so nothing is lost.
    fn handle_bed_caps(&self, caps: &gst::CapsRef) {
        let Some(structure) = caps.structure(0) else {
            return;
        };
        let Ok(channels) = structure.get::<i32>("channels") else {
            return;
        };
        let bed_channels = channels as u32;
        self.bed_channels.store(bed_channels, Ordering::Relaxed);

        let mut state_guard = lock_recover(&self.state);
        let already_current = state_guard
            .as_ref()
            .is_some_and(|state| state.processor.bed_channels() == bed_channels);
        if already_current {
            return;
        }

        let settings = *lock_recover(&self.settings);
        let mut new_state = match ProcessingState::new(&settings, bed_channels) {
            Ok(state) => state,
            Err(err) => {
                gst::element_error!(
                    self.obj(),
                    gst::CoreError::Negotiation,
                    ["cannot initialize automix processor for a {bed_channels}ch bed: {err:?}"]
                );
                return;
            }
        };

        let mut pending_dialogue = lock_recover(&self.pending_dialogue_before_state);
        if !pending_dialogue.is_empty() {
            let _ = new_state.processor.feed_dialogue(&pending_dialogue);
            new_state.mix_dialogue_pending.extend_from_slice(&pending_dialogue);
            pending_dialogue.clear();
        }
        drop(pending_dialogue);

        *state_guard = Some(new_state);
    }

    /// Forwards one incoming Bed chunk (whatever size the upstream delivered) to `is_leveled_src`
    /// immediately, after applying this element's continuous gain ramp - no waiting for a full
    /// tick to accumulate first (see the module doc comment on latency/control-rate decoupling).
    /// Also feeds the (already-gained) chunk into `bed_meter` and the mix-alignment buffer, and
    /// gives the control step a chance to run.
    ///
    /// `state` must already be `Some` by the time this runs: GStreamer guarantees Bed's own Caps
    /// event precedes Bed's own first buffer, and `handle_bed_caps` constructs `state` right then.
    fn process_bed_chunk(&self, raw_bed: &[f32]) -> Result<gst::FlowSuccess, gst::FlowError> {
        // No new Dialogue audio arrived in this call - just read whatever `voice_activity` last
        // settled on (see `voice_active`'s doc comment for the locking-order reason this is read
        // *before* `state` is locked below, not while it's held).
        let voice_active = self.voice_active();
        // The IS detector gets this chunk's *dry* Bed (before `apply_gain_to_bed_chunk` below), so
        // detection never depends on how hard the automix is currently ducking.
        let is_voice_active = self.feed_is_voice_activity(raw_bed);
        let mut leveled_bed = raw_bed.to_vec();

        let (mix_ready, bed_channels) = {
            let mut state_guard = lock_recover(&self.state);
            let state = state_guard.as_mut().ok_or_else(|| {
                gst::element_error!(
                    self.obj(),
                    gst::CoreError::Negotiation,
                    ["received a bed buffer before bed_sink's caps event"]
                );
                gst::FlowError::Error
            })?;
            // Feed-forward (Specs/TechnicalConcept.md section 5.2): `process_bed` measures the dry
            // signal, steps the fast loop stage and applies the resulting gain in sub-chunks, then
            // applies the lookahead delay - all inside the shared core.
            state.processor.process_bed(&mut leveled_bed).map_err(|err| {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Failed,
                    ["bed loudness meter rejected {} frames: {err:?}", leveled_bed.len()]
                );
                gst::FlowError::Error
            })?;
            state.mix_bed_pending.extend_from_slice(&leveled_bed);

            state.processor.maybe_run_control_step(voice_active, is_voice_active);

            (drain_mix_ready(state), state.processor.bed_channels())
        };

        let leveled_frames = leveled_bed.len() as u64 / bed_channels as u64;
        let is_leveled_frame_index = self.allocate_frame_index(|f| &mut f.is_leveled, leveled_frames);
        self.is_leveled_src
            .push(f32_slice_to_buffer(&leveled_bed, bed_channels, is_leveled_frame_index))
            .map_err(|_| gst::FlowError::Error)?;

        self.push_mix_if_ready(mix_ready, bed_channels)
    }

    /// Forwards one incoming Dialogue chunk to `dialogue_src` immediately, dry (Dialogue is never
    /// gained) - this happens unconditionally, even before Bed's caps (and therefore `state`) are
    /// known, matching the near-zero-latency principle. Metering/mix-alignment, however, need
    /// `state`: if it isn't ready yet, the raw samples are buffered in
    /// `pending_dialogue_before_state` for `handle_bed_caps` to replay once Bed's caps arrive.
    /// Voice-activity detection, unlike metering, is fed unconditionally too, `state` or not (see
    /// `voice_activity`'s doc comment) - Dialogue's sample rate is fixed, so there's nothing to
    /// wait on there.
    fn process_dialogue_chunk(&self, raw_dialogue: &[f32]) -> Result<gst::FlowSuccess, gst::FlowError> {
        let voice_active = self.feed_voice_activity(raw_dialogue);
        let is_voice_active = self.is_voice_active();

        let (mix_ready, bed_channels) = {
            let mut state_guard = lock_recover(&self.state);
            match state_guard.as_mut() {
                Some(state) => {
                    state.processor.feed_dialogue(raw_dialogue).map_err(|err| {
                        gst::element_error!(
                            self.obj(),
                            gst::StreamError::Failed,
                            ["dialogue loudness meter rejected {} frames: {err:?}", raw_dialogue.len()]
                        );
                        gst::FlowError::Error
                    })?;
                    state.mix_dialogue_pending.extend_from_slice(raw_dialogue);

                    state.processor.maybe_run_control_step(voice_active, is_voice_active);

                    (drain_mix_ready(state), Some(state.processor.bed_channels()))
                }
                None => {
                    lock_recover(&self.pending_dialogue_before_state).extend_from_slice(raw_dialogue);
                    (None, None)
                }
            }
        };

        let dialogue_frame_index = self.allocate_frame_index(|f| &mut f.dialogue, raw_dialogue.len() as u64);
        self.dialogue_src
            .push(f32_slice_to_buffer(raw_dialogue, 1, dialogue_frame_index))
            .map_err(|_| gst::FlowError::Error)?;

        match bed_channels {
            Some(bed_channels) => self.push_mix_if_ready(mix_ready, bed_channels),
            None => Ok(gst::FlowSuccess::Ok),
        }
    }

    fn push_mix_if_ready(&self, mix_ready: Option<Vec<f32>>, bed_channels: u32) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let Some(mix) = mix_ready {
            let mix_frames = mix.len() as u64 / bed_channels as u64;
            let mix_frame_index = self.allocate_frame_index(|f| &mut f.mix, mix_frames);
            self.mix_src
                .push(f32_slice_to_buffer(&mix, bed_channels, mix_frame_index))
                .map_err(|_| gst::FlowError::Error)?;
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, is_bed: bool, event: gst::Event) -> bool {
        use gst::EventView;
        match event.view() {
            EventView::Eos(_) => self.handle_sink_eos(is_bed, event),
            EventView::Caps(c) => {
                if is_bed {
                    self.handle_bed_caps(c.caps());
                    let r1 = self.is_leveled_src.push_event(event.clone());
                    let r2 = self.mix_src.push_event(event);
                    r1 && r2
                } else {
                    self.dialogue_src.push_event(event)
                }
            }
            EventView::StreamStart(_) | EventView::Segment(_) => {
                if is_bed {
                    let r1 = self.is_leveled_src.push_event(event.clone());
                    let r2 = self.mix_src.push_event(event);
                    r1 && r2
                } else {
                    self.dialogue_src.push_event(event)
                }
            }
            _ => {
                if is_bed {
                    self.is_leveled_src.push_event(event.clone()) && self.mix_src.push_event(event)
                } else {
                    self.dialogue_src.push_event(event)
                }
            }
        }
    }

    /// `is_leveled_src`/`dialogue_src` end as soon as their own sink's EOS arrives - each is
    /// independent of the other now, so there's no reason to hold one back waiting for the other
    /// side. `mix_src` can only end once *both* sinks are done (see
    /// `flush_remaining_mix_tail`'s doc comment for how any still-unpaired tail is handled then).
    fn handle_sink_eos(&self, is_bed: bool, event: gst::Event) -> bool {
        if is_bed {
            self.bed_eos.store(true, Ordering::Relaxed);
        } else {
            self.dialogue_eos.store(true, Ordering::Relaxed);
        }
        let both_eos = self.bed_eos.load(Ordering::Relaxed) && self.dialogue_eos.load(Ordering::Relaxed);

        // If Bed's caps never arrived (so `state` is still `None`), there's nothing to flush -
        // e.g. a Bed sink that reached EOS with zero buffers ever sent.
        let final_mix = if both_eos {
            lock_recover(&self.state)
                .as_mut()
                .and_then(|state| flush_remaining_mix_tail(state).map(|mix| (mix, state.processor.bed_channels())))
        } else {
            None
        };

        if let Some((mix, bed_channels)) = final_mix {
            let mix_frames = mix.len() as u64 / bed_channels as u64;
            let mix_frame_index = self.allocate_frame_index(|f| &mut f.mix, mix_frames);
            let _ = self.mix_src.push(f32_slice_to_buffer(&mix, bed_channels, mix_frame_index));
        }

        let own_src_ok = if is_bed {
            self.is_leveled_src.push_event(event.clone())
        } else {
            self.dialogue_src.push_event(event.clone())
        };

        if both_eos {
            own_src_ok && self.mix_src.push_event(event)
        } else {
            // Swallow for now - the other side is still producing data, so mix_src can't end yet.
            // Not an error: this sink has legitimately finished.
            own_src_ok
        }
    }

    /// Answers `LATENCY` queries arriving on a src pad by querying upstream through this
    /// element's sink pad(s) and returning that unchanged. This element no longer holds audio back
    /// before forwarding it (see the module doc comment on latency/control-rate decoupling), so
    /// there's nothing of its own to add - but the override is still required: our src pads
    /// deliberately report no internal links (see the cycle-avoidance note in `with_class`), so
    /// without this, `gst_pad_query_default`'s internal-links-based forwarding couldn't discover
    /// upstream latency through this element at all, and the query would just fail.
    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;
        match query.view_mut() {
            QueryViewMut::Latency(q) => {
                let mut bed_query = gst::query::Latency::new();
                let mut dialogue_query = gst::query::Latency::new();
                let bed_ok = self.bed_sink.peer_query(&mut bed_query);
                let dialogue_ok = self.dialogue_sink.peer_query(&mut dialogue_query);
                if !bed_ok && !dialogue_ok {
                    return false;
                }
                let (bed_live, bed_min, bed_max) = if bed_ok {
                    bed_query.result()
                } else {
                    (false, gst::ClockTime::ZERO, None)
                };
                let (dialogue_live, dialogue_min, dialogue_max) = if dialogue_ok {
                    dialogue_query.result()
                } else {
                    (false, gst::ClockTime::ZERO, None)
                };

                let live = bed_live || dialogue_live;

                // Whatever lookahead is configured is genuinely held back before output, so it is
                // this element's own added latency and must be reported. At the default of 0 this
                // adds nothing and the element keeps the zero-added-latency property the OB-van
                // deployment depends on (see the module header). Under-reporting it is not a
                // cosmetic bug: a synced sink schedules rendering from the pipeline's total
                // reported latency, and getting it wrong caused audible periodic glitching once
                // before.
                let own_latency = gst::ClockTime::from_nseconds(
                    lock_recover(&self.settings).lookahead_ms as u64 * 1_000_000,
                );

                let min = bed_min.max(dialogue_min) + own_latency;
                let max = match (bed_max, dialogue_max) {
                    (Some(a), Some(b)) => Some(a.min(b) + own_latency),
                    (Some(a), None) => Some(a + own_latency),
                    (None, Some(b)) => Some(b + own_latency),
                    (None, None) => None,
                };
                q.set(live, min, max);
                true
            }
            _ => {
                let obj = self.obj();
                gst::Pad::query_default(pad, Some(&*obj), query)
            }
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ComISAssist {
    const NAME: &'static str = "GstComISAssist";
    type Type = super::ComISAssist;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let bed_sink_templ = klass.pad_template("bed_sink").unwrap();
        let dialogue_sink_templ = klass.pad_template("dialogue_sink").unwrap();
        let is_leveled_src_templ = klass.pad_template("is_leveled_src").unwrap();
        let dialogue_src_templ = klass.pad_template("dialogue_src").unwrap();
        let mix_src_templ = klass.pad_template("mix_src").unwrap();

        // Explicit internal-links mappings (sink -> the src pad(s) its data reaches, and back)
        // are required here: GstPad's *default* iterate_internal_links treats every pad of the
        // opposite direction on an element as linked to every other one, which for 2 sinks / 3
        // srcs creates a real cycle (bed_sink <-> dialogue_src <-> dialogue_sink <-> is_leveled_src
        // <-> ...) that latency/scheduling queries then loop through forever. Found by actually
        // running a pipeline, not by building - a plain `cargo build` success says nothing about
        // this. Keeping the mapping bipartite (sink->src / src->sink only, never sink->sink or
        // src->src) is what breaks the cycle: a query crosses this element exactly once per pad.
        let bed_sink = gst::Pad::builder_from_template(&bed_sink_templ)
            .chain_function(|_pad, parent, buffer| {
                Self::catch_panic_pad_function(
                    parent,
                    || {
                        eprintln!("[comisassist] PANIC caught in bed_sink chain function - see backtrace above");
                        Err(gst::FlowError::Error)
                    },
                    |this| this.sink_chain(true, &buffer),
                )
            })
            .event_function(|_pad, parent, event| {
                Self::catch_panic_pad_function(
                    parent,
                    || {
                        eprintln!("[comisassist] PANIC caught in bed_sink event function - see backtrace above");
                        false
                    },
                    |this| this.sink_event(true, event),
                )
            })
            .iterate_internal_links_function(|_pad, parent| {
                Self::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(vec![]),
                    |this| gst::Iterator::from_vec(vec![this.is_leveled_src.clone(), this.mix_src.clone()]),
                )
            })
            .build();

        let dialogue_sink = gst::Pad::builder_from_template(&dialogue_sink_templ)
            .chain_function(|_pad, parent, buffer| {
                Self::catch_panic_pad_function(
                    parent,
                    || {
                        eprintln!("[comisassist] PANIC caught in dialogue_sink chain function - see backtrace above");
                        Err(gst::FlowError::Error)
                    },
                    |this| this.sink_chain(false, &buffer),
                )
            })
            .event_function(|_pad, parent, event| {
                Self::catch_panic_pad_function(
                    parent,
                    || {
                        eprintln!("[comisassist] PANIC caught in dialogue_sink event function - see backtrace above");
                        false
                    },
                    |this| this.sink_event(false, event),
                )
            })
            .iterate_internal_links_function(|_pad, parent| {
                Self::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(vec![]),
                    |this| gst::Iterator::from_vec(vec![this.dialogue_src.clone(), this.mix_src.clone()]),
                )
            })
            .build();

        // Src pads report NO internal links back to the sinks, deliberately one-directional.
        // `gst_pad_forward` (used e.g. for latency-event redistribution) walks internal-links as
        // plain undirected adjacency with no visited-set/cycle detection - so *any* reciprocal
        // pair (bed_sink -> is_leveled_src declared alongside is_leveled_src -> bed_sink) is
        // already a 2-node cycle by itself, independent of mix_src's fan-in. Found by tracing
        // exactly where a live pipeline hung (it looped forever redistributing latency events),
        // not by inspection. Keeping the mapping one-directional (sink -> the srcs it feeds only)
        // makes the whole internal-links graph a simple, cycle-free bipartite structure. The only
        // cost: a query/event starting from a src pad and looking to auto-discover this
        // element's upstream via internal-links won't find anything that way - acceptable, since
        // actual data/event flow is handled explicitly by this element's own chain/event
        // functions, not by this generic mechanism.
        // Explicit *empty* internal-links (not just omitting the function - that would fall back
        // to GStreamer's default, which is "every pad of the opposite direction on this element",
        // i.e. exactly the original all-linked cycle this whole approach is designed to avoid).
        let no_internal_links = |_pad: &gst::Pad, _parent: Option<&gst::Object>| gst::Iterator::from_vec(vec![]);
        // `query_function` compensates for the empty internal-links above specifically for
        // LATENCY queries (see `src_query`'s doc comment) - everything else falls through to the
        // pad default.
        let is_leveled_src = gst::Pad::builder_from_template(&is_leveled_src_templ)
            .iterate_internal_links_function(no_internal_links)
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(parent, || false, |this| this.src_query(pad, query))
            })
            .build();
        let dialogue_src = gst::Pad::builder_from_template(&dialogue_src_templ)
            .iterate_internal_links_function(no_internal_links)
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(parent, || false, |this| this.src_query(pad, query))
            })
            .build();
        let mix_src = gst::Pad::builder_from_template(&mix_src_templ)
            .iterate_internal_links_function(no_internal_links)
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(parent, || false, |this| this.src_query(pad, query))
            })
            .build();

        let settings = Settings::default();
        // `.ok()`, not `.expect(...)`: if this fails (e.g. the ONNX Runtime binary couldn't be
        // obtained), the element still comes up and falls back to always-voice-active (see
        // `voice_active`'s doc comment) rather than refusing to load entirely.
        let voice_activity = SileroVad::new(SAMPLE_RATE, VoiceActivityConfig::default())
            .inspect_err(|err| eprintln!("[comisassist] voice-activity detection unavailable, failing open: {err}"))
            .ok();
        // A second, independent detector instance for the Bed side - see `is_voice_activity`.
        let is_voice_activity = SileroVad::new(SAMPLE_RATE, VoiceActivityConfig::default()).ok();
        Self {
            state: Mutex::new(None),
            pending_dialogue_before_state: Mutex::new(Vec::new()),
            bed_eos: AtomicBool::new(false),
            dialogue_eos: AtomicBool::new(false),
            bed_channels: AtomicU32::new(0),
            voice_activity: Mutex::new(voice_activity),
            is_voice_activity: Mutex::new(is_voice_activity),
            settings: Mutex::new(settings),
            frame_indices: Mutex::new(FrameIndices::default()),
            bed_sink,
            dialogue_sink,
            is_leveled_src,
            dialogue_src,
            mix_src,
        }
    }
}

impl ObjectImpl for ComISAssist {
    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.add_pad(&self.bed_sink).unwrap();
        obj.add_pad(&self.dialogue_sink).unwrap();
        obj.add_pad(&self.is_leveled_src).unwrap();
        obj.add_pad(&self.dialogue_src).unwrap();
        obj.add_pad(&self.mix_src).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
        PROPERTIES.get_or_init(|| {
            vec![
                glib::ParamSpecDouble::builder("target-ratio")
                    .default_value(AutomixEngineConfig::default().target_ratio_lu)
                    .build(),
                glib::ParamSpecDouble::builder("overvoice-ratio")
                    .default_value(AutomixEngineConfig::default().overvoice_ratio_lu)
                    .build(),
                glib::ParamSpecDouble::builder("speed")
                    .minimum(0.25)
                    .maximum(4.0)
                    .default_value(AutomixEngineConfig::default().speed)
                    .build(),
                glib::ParamSpecDouble::builder("lookahead-ms")
                    .minimum(0.0)
                    .maximum(20.0)
                    .default_value(0.0)
                    .build(),
                glib::ParamSpecBoolean::builder("interview-passthrough-enable")
                    .default_value(AutomixEngineConfig::default().interview_passthrough_enabled)
                    .build(),
                glib::ParamSpecDouble::builder("max-gain-reduction-db")
                    .default_value(AutomixEngineConfig::default().max_gain_reduction_db)
                    .build(),
                glib::ParamSpecBoolean::builder("automix-enable")
                    .default_value(true)
                    .build(),
                glib::ParamSpecDouble::builder("divergence")
                    .minimum(0.0)
                    .maximum(100.0)
                    .default_value(Settings::default().divergence_percent)
                    .build(),
                glib::ParamSpecDouble::builder("current-gain-reduction-db")
                    .default_value(0.0)
                    .read_only()
                    .build(),
                glib::ParamSpecDouble::builder("current-ratio-lu")
                    .default_value(0.0)
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("voice-active")
                    .default_value(false)
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("voice-active-is")
                    .default_value(false)
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("current-mix-state")
                    .default_value(Some("release"))
                    .read_only()
                    .build(),
            ]
        })
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "target-ratio" => lock_recover(&self.settings).target_ratio_lu.to_value(),
            "overvoice-ratio" => lock_recover(&self.settings).overvoice_ratio_lu.to_value(),
            "speed" => lock_recover(&self.settings).speed.to_value(),
            "lookahead-ms" => lock_recover(&self.settings).lookahead_ms.to_value(),
            "interview-passthrough-enable" => {
                lock_recover(&self.settings).interview_passthrough_enabled.to_value()
            }
            "max-gain-reduction-db" => lock_recover(&self.settings).max_gain_reduction_db.to_value(),
            "automix-enable" => lock_recover(&self.settings).automix_enabled.to_value(),
            "divergence" => lock_recover(&self.settings).divergence_percent.to_value(),
            "current-gain-reduction-db" => lock_recover(&self.state)
                .as_ref()
                .map_or(0.0, |state| state.processor.applied_gain_reduction_db())
                .to_value(),
            "current-ratio-lu" => {
                // Cheap diagnostic read; not stored separately from the engine's own state.
                0.0f64.to_value()
            }
            "voice-active" => self.voice_active().to_value(),
            "voice-active-is" => self.is_voice_active().to_value(),
            "current-mix-state" => lock_recover(&self.state)
                .as_ref()
                .map_or("release", |state| match state.processor.mix_state() {
                    MixState::ReleaseToUnity => "release",
                    MixState::DuckToTarget => "duck-target",
                    MixState::InterviewPassthrough => "interview",
                    MixState::DuckToOvervoice => "duck-overvoice",
                })
                .to_value(),
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut lookahead_changed = false;
        // Updates the running `AutomixProcessor` in place via `set_config` (added once
        // `AutomixEngine`/`GainComputer` gained live-reconfiguration support) rather than
        // rebuilding `ProcessingState` from scratch - preserves accumulated loudness-meter
        // history, the in-progress gain ramp, and any pending mix-alignment buffers, none of
        // which a full rebuild could keep.
        //
        // The `settings` lock is scoped to end before `state` is ever locked (never held
        // together) - this must stay a *sequential* pair of locks, not nested, since
        // `handle_bed_caps` (running concurrently on `bed_sink`'s streaming thread) locks `state`
        // first and `settings` second; holding them in the opposite order here would be a classic
        // lock-order-inversion deadlock risk.
        let settings_snapshot = {
            let mut settings = lock_recover(&self.settings);
            match pspec.name() {
                "target-ratio" => settings.target_ratio_lu = value.get().unwrap(),
                "overvoice-ratio" => settings.overvoice_ratio_lu = value.get().unwrap(),
                    "speed" => settings.speed = value.get().unwrap(),
                "lookahead-ms" => {
                    settings.lookahead_ms = value.get().unwrap();
                    lookahead_changed = true;
                }
                "interview-passthrough-enable" => {
                    settings.interview_passthrough_enabled = value.get().unwrap()
                }
                "max-gain-reduction-db" => settings.max_gain_reduction_db = value.get().unwrap(),
                "divergence" => settings.divergence_percent = value.get().unwrap(),
                "automix-enable" => settings.automix_enabled = value.get().unwrap(),
                _ => unimplemented!(),
            }
            *settings
        };

        if lookahead_changed {
            // The element's own latency contribution just changed, so the pipeline has to
            // re-run its latency negotiation - otherwise synced sinks keep scheduling against
            // the previous value.
            let _ = self.obj().post_message(gst::message::Latency::builder().src(&*self.obj()).build());
        }

        // Nothing to update yet if Bed's caps haven't arrived (see `handle_bed_caps`) - the fresh
        // `Settings` will be picked up whenever it first constructs `ProcessingState`.
        if let Some(state) = lock_recover(&self.state).as_mut() {
            state.processor.set_config(settings_snapshot.automix_config());
            state.processor.set_lookahead_seconds(settings_snapshot.lookahead_ms / 1000.0);
            state.processor.set_automix_enabled(settings_snapshot.automix_enabled);
            // Not part of `AutomixEngineConfig` (it's a GStreamer-wrapper-only concept, not shared
            // core config), so `set_config` above doesn't touch it.
            state.divergence = (settings_snapshot.divergence_percent / 100.0) as f32;
        }
    }
}

impl GstObjectImpl for ComISAssist {}

impl ElementImpl for ComISAssist {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
        Some(ELEMENT_METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "Com-IS-Assist",
                "Filter/Effect/Audio",
                "Broadcast COM/IS ratio-driven Bed automixer",
                "Com-IS-Assist",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
        PAD_TEMPLATES.get_or_init(|| {
            let bed_caps = ranged_audio_caps(MIN_BED_CHANNELS, MAX_BED_CHANNELS);
            let dialogue_caps = fixed_audio_caps(1);
            vec![
                gst::PadTemplate::new(
                    "bed_sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &bed_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "dialogue_sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &dialogue_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "is_leveled_src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &bed_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "dialogue_src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &dialogue_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "mix_src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &bed_caps,
                )
                .unwrap(),
            ]
        })
    }
}

// GstComISAssist: 2 sink pads (bed_sink, dialogue_sink) / 3 src pads (is_leveled_src,
// dialogue_src, mix_src), wired to com_is_assist_core's closed-loop automix (Ebur128Meter -> RatioEngine
// -> AutomixEngine). See Specs/TechnicalConcept.md section 7 for the design, and section 4 for
// why the Bed meter must observe the already-gained signal.
//
// Scope of this first pass (deliberately, not yet the full product): fixed caps (48kHz, stereo
// Bed / mono Dialogue - matching this project's test fixtures) rather than full 2-6ch dynamic
// negotiation, and pad synchronization is hand-rolled (per-pad accumulation buffers) rather than
// GstCollectPads, which gstreamer-rs 0.25 doesn't wrap safely.
//
// Output latency is deliberately decoupled from the automix control rate: audio is forwarded to
// is_leveled_src/dialogue_src the moment it arrives (near-zero added latency), and mix_src as soon
// as both sides have anything to pair (bounded only by upstream arrival jitter between the two
// sinks, not by a fixed wait). The ratio/gain *target* is still only recomputed once per
// ~TICK_SECONDS (matching BS.1770's own 100ms gating-block granularity - ticking faster wouldn't
// yield more frequent loudness information anyway), but that target is approached via a continuous
// per-frame ramp rather than a per-tick batch, so nothing has to wait for a whole tick to buffer
// before being forwarded. See `ProcessingState` below.

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor, GainComputerConfig};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
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
const BED_CHANNELS: u32 = 2;

fn audio_caps(channels: i32) -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("rate", SAMPLE_RATE as i32)
        .field("channels", channels)
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
    max_tolerance_lu: f64,
    min_tolerance_lu: f64,
    max_gain_reduction_db: f64,
    step_db_per_lu: f64,
    attack_seconds: f64,
    hold_seconds: f64,
    release_seconds: f64,
    automix_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let automix = AutomixEngineConfig::default();
        let gain = GainComputerConfig::default();
        Self {
            target_ratio_lu: automix.target_ratio_lu,
            max_tolerance_lu: automix.max_tolerance_lu,
            min_tolerance_lu: automix.min_tolerance_lu,
            max_gain_reduction_db: automix.max_gain_reduction_db,
            step_db_per_lu: automix.step_db_per_lu,
            attack_seconds: gain.attack_seconds,
            hold_seconds: gain.hold_seconds,
            release_seconds: gain.release_seconds,
            automix_enabled: true,
        }
    }
}

impl Settings {
    fn automix_config(&self) -> AutomixEngineConfig {
        AutomixEngineConfig {
            target_ratio_lu: self.target_ratio_lu,
            max_tolerance_lu: self.max_tolerance_lu,
            min_tolerance_lu: self.min_tolerance_lu,
            max_gain_reduction_db: self.max_gain_reduction_db,
            step_db_per_lu: self.step_db_per_lu,
        }
    }

    fn gain_computer_config(&self) -> GainComputerConfig {
        GainComputerConfig {
            attack_seconds: self.attack_seconds,
            hold_seconds: self.hold_seconds,
            release_seconds: self.release_seconds,
            max_rate_db_per_s: None,
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

    /// Bed (already gained) and Dialogue samples waiting to be paired for `mix_src`. Drained
    /// immediately whenever *both* have at least one frame available (see `drain_mix_ready`) -
    /// there's no fixed-size wait here, so this only ever holds however much the two sinks'
    /// arrival timing happens to be out of step by, not a designed buffering delay.
    mix_bed_pending: Vec<f32>,
    mix_dialogue_pending: Vec<f32>,

    /// Set once the corresponding sink has received EOS. `is_leveled_src`/`dialogue_src` each end
    /// as soon as their own sink's EOS arrives (they have no cross-dependency), but `mix_src` can
    /// only end once *both* are set - see `flush_remaining_mix_tail`'s doc comment for how any
    /// still-unpaired tail on the longer side is handled at that point.
    bed_eos: bool,
    dialogue_eos: bool,
}

impl ProcessingState {
    fn new(settings: &Settings) -> Self {
        Self {
            processor: AutomixProcessor::new(
                BED_CHANNELS,
                SAMPLE_RATE,
                settings.automix_config(),
                settings.gain_computer_config(),
                TICK_SECONDS,
            )
            .expect("valid processor config"),
            mix_bed_pending: Vec::new(),
            mix_dialogue_pending: Vec::new(),
            bed_eos: false,
            dialogue_eos: false,
        }
    }

}

/// Drains whatever Bed/Dialogue audio *can* currently be paired for `mix_src` - i.e. up to
/// `min(bed_pending, dialogue_pending)` frames - immediately, with no fixed-size wait. Returns
/// `None` if either side is currently empty; that's normal (just means one side is momentarily
/// ahead) and does not mean the stream has ended.
fn drain_mix_ready(state: &mut ProcessingState) -> Option<Vec<f32>> {
    let bed_frames = state.mix_bed_pending.len() / BED_CHANNELS as usize;
    let dialogue_frames = state.mix_dialogue_pending.len();
    let frames = bed_frames.min(dialogue_frames);
    if frames == 0 {
        return None;
    }
    let bed_chunk: Vec<f32> = state.mix_bed_pending.drain(0..frames * BED_CHANNELS as usize).collect();
    let dialogue_chunk: Vec<f32> = state.mix_dialogue_pending.drain(0..frames).collect();
    let mut mix = bed_chunk;
    for (frame, dialogue_sample) in dialogue_chunk.iter().enumerate() {
        for channel in 0..BED_CHANNELS as usize {
            mix[frame * BED_CHANNELS as usize + channel] += dialogue_sample;
        }
    }
    Some(mix)
}

/// Called only once *both* sinks have reached EOS: flushes whatever's left on the longer side,
/// treating the shorter (already-finished) side as silent for that remaining tail. This is the
/// only place "mix alone" is correct - `drain_mix_ready` already continuously drains anything that
/// *can* be paired as it arrives, so by the time both sides are done, any leftover on one side
/// means the other side's stream was simply shorter, not that pairing was skipped.
fn flush_remaining_mix_tail(state: &mut ProcessingState) -> Option<Vec<f32>> {
    let bed_frames = state.mix_bed_pending.len() / BED_CHANNELS as usize;
    if bed_frames > 0 {
        let bed_chunk: Vec<f32> = state.mix_bed_pending.drain(..).collect();
        state.mix_dialogue_pending.clear();
        return Some(bed_chunk);
    }
    let dialogue_frames = state.mix_dialogue_pending.len();
    if dialogue_frames > 0 {
        let dialogue_chunk: Vec<f32> = state.mix_dialogue_pending.drain(..).collect();
        state.mix_bed_pending.clear();
        let mut mix = vec![0.0f32; dialogue_chunk.len() * BED_CHANNELS as usize];
        for (frame, sample) in dialogue_chunk.iter().enumerate() {
            for channel in 0..BED_CHANNELS as usize {
                mix[frame * BED_CHANNELS as usize + channel] = *sample;
            }
        }
        return Some(mix);
    }
    None
}

#[cfg(test)]
mod mix_alignment_tests {
    use super::*;

    #[test]
    fn pairs_equal_length_bed_and_dialogue_by_summing() {
        let mut state = ProcessingState::new(&Settings::default());
        state.mix_bed_pending = vec![0.1, 0.2, 0.3, 0.4]; // 2 stereo frames
        state.mix_dialogue_pending = vec![1.0, 2.0]; // 2 mono frames

        let mix = drain_mix_ready(&mut state).expect("both sides have data");
        assert_eq!(mix, vec![1.1, 1.2, 2.3, 2.4]);
        assert!(state.mix_bed_pending.is_empty());
        assert!(state.mix_dialogue_pending.is_empty());
    }

    #[test]
    fn returns_none_when_either_side_is_empty() {
        let mut state = ProcessingState::new(&Settings::default());
        state.mix_bed_pending = vec![0.1, 0.2];
        assert!(drain_mix_ready(&mut state).is_none());

        let mut state = ProcessingState::new(&Settings::default());
        state.mix_dialogue_pending = vec![1.0];
        assert!(drain_mix_ready(&mut state).is_none());
    }

    #[test]
    fn drains_only_the_minimum_available_leaving_the_rest_pending() {
        let mut state = ProcessingState::new(&Settings::default());
        state.mix_bed_pending = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6]; // 3 stereo frames
        state.mix_dialogue_pending = vec![1.0]; // 1 mono frame

        let mix = drain_mix_ready(&mut state).expect("one frame is pairable");
        assert_eq!(mix, vec![1.1, 1.2]);
        assert_eq!(state.mix_bed_pending, vec![0.3, 0.4, 0.5, 0.6]);
        assert!(state.mix_dialogue_pending.is_empty());
    }

    #[test]
    fn flush_tail_treats_leftover_bed_as_mix_alone() {
        let mut state = ProcessingState::new(&Settings::default());
        state.mix_bed_pending = vec![0.1, 0.2, 0.3, 0.4];
        state.mix_dialogue_pending.clear();

        let mix = flush_remaining_mix_tail(&mut state).expect("bed has a leftover tail");
        assert_eq!(mix, vec![0.1, 0.2, 0.3, 0.4]);
        assert!(state.mix_bed_pending.is_empty());
    }

    #[test]
    fn flush_tail_expands_leftover_dialogue_across_bed_channels() {
        let mut state = ProcessingState::new(&Settings::default());
        state.mix_dialogue_pending = vec![1.0, 2.0];

        let mix = flush_remaining_mix_tail(&mut state).expect("dialogue has a leftover tail");
        assert_eq!(mix, vec![1.0, 1.0, 2.0, 2.0]);
        assert!(state.mix_dialogue_pending.is_empty());
    }

    #[test]
    fn flush_tail_is_none_when_nothing_is_pending() {
        let mut state = ProcessingState::new(&Settings::default());
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
    state: Mutex<ProcessingState>,
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

    /// Forwards one incoming Bed chunk (whatever size the upstream delivered) to `is_leveled_src`
    /// immediately, after applying this element's continuous gain ramp - no waiting for a full
    /// tick to accumulate first (see the module doc comment on latency/control-rate decoupling).
    /// Also feeds the (already-gained) chunk into `bed_meter` and the mix-alignment buffer, and
    /// gives the control step a chance to run.
    fn process_bed_chunk(&self, raw_bed: &[f32]) -> Result<gst::FlowSuccess, gst::FlowError> {
        let automix_enabled = lock_recover(&self.settings).automix_enabled;
        let mut leveled_bed = raw_bed.to_vec();

        let mix_ready = {
            let mut state = lock_recover(&self.state);
            state.processor.apply_gain_to_bed_chunk(&mut leveled_bed);

            // Closed loop (Specs/TechnicalConcept.md section 4): the Bed meter observes the
            // already-gained signal, not the dry one.
            state.processor.feed_bed(&leveled_bed).map_err(|err| {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Failed,
                    ["bed loudness meter rejected {} frames: {err:?}", leveled_bed.len()]
                );
                gst::FlowError::Error
            })?;
            state.mix_bed_pending.extend_from_slice(&leveled_bed);

            state.processor.maybe_run_control_step(automix_enabled);

            drain_mix_ready(&mut state)
        };

        let leveled_frames = leveled_bed.len() as u64 / BED_CHANNELS as u64;
        let is_leveled_frame_index = self.allocate_frame_index(|f| &mut f.is_leveled, leveled_frames);
        self.is_leveled_src
            .push(f32_slice_to_buffer(&leveled_bed, BED_CHANNELS, is_leveled_frame_index))
            .map_err(|_| gst::FlowError::Error)?;

        self.push_mix_if_ready(mix_ready)
    }

    /// Forwards one incoming Dialogue chunk to `dialogue_src` immediately, dry (Dialogue is never
    /// gained). Also feeds it into `dialogue_meter` and the mix-alignment buffer, and gives the
    /// control step a chance to run.
    fn process_dialogue_chunk(&self, raw_dialogue: &[f32]) -> Result<gst::FlowSuccess, gst::FlowError> {
        let automix_enabled = lock_recover(&self.settings).automix_enabled;

        let mix_ready = {
            let mut state = lock_recover(&self.state);
            state.processor.feed_dialogue(raw_dialogue).map_err(|err| {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Failed,
                    ["dialogue loudness meter rejected {} frames: {err:?}", raw_dialogue.len()]
                );
                gst::FlowError::Error
            })?;
            state.mix_dialogue_pending.extend_from_slice(raw_dialogue);

            state.processor.maybe_run_control_step(automix_enabled);

            drain_mix_ready(&mut state)
        };

        let dialogue_frame_index = self.allocate_frame_index(|f| &mut f.dialogue, raw_dialogue.len() as u64);
        self.dialogue_src
            .push(f32_slice_to_buffer(raw_dialogue, 1, dialogue_frame_index))
            .map_err(|_| gst::FlowError::Error)?;

        self.push_mix_if_ready(mix_ready)
    }

    fn push_mix_if_ready(&self, mix_ready: Option<Vec<f32>>) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let Some(mix) = mix_ready {
            let mix_frames = mix.len() as u64 / BED_CHANNELS as u64;
            let mix_frame_index = self.allocate_frame_index(|f| &mut f.mix, mix_frames);
            self.mix_src
                .push(f32_slice_to_buffer(&mix, BED_CHANNELS, mix_frame_index))
                .map_err(|_| gst::FlowError::Error)?;
        }
        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, is_bed: bool, event: gst::Event) -> bool {
        use gst::EventView;
        match event.view() {
            EventView::Eos(_) => self.handle_sink_eos(is_bed, event),
            EventView::StreamStart(_) | EventView::Caps(_) | EventView::Segment(_) => {
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
        let (both_eos, final_mix) = {
            let mut state = lock_recover(&self.state);
            if is_bed {
                state.bed_eos = true;
            } else {
                state.dialogue_eos = true;
            }
            let both = state.bed_eos && state.dialogue_eos;
            let final_mix = if both { flush_remaining_mix_tail(&mut state) } else { None };
            (both, final_mix)
        };

        if let Some(mix) = final_mix {
            let mix_frames = mix.len() as u64 / BED_CHANNELS as u64;
            let mix_frame_index = self.allocate_frame_index(|f| &mut f.mix, mix_frames);
            let _ = self.mix_src.push(f32_slice_to_buffer(&mix, BED_CHANNELS, mix_frame_index));
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
                let min = bed_min.max(dialogue_min);
                let max = match (bed_max, dialogue_max) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
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
        Self {
            state: Mutex::new(ProcessingState::new(&settings)),
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
                glib::ParamSpecDouble::builder("max-tolerance")
                    .default_value(AutomixEngineConfig::default().max_tolerance_lu)
                    .build(),
                glib::ParamSpecDouble::builder("min-tolerance")
                    .default_value(AutomixEngineConfig::default().min_tolerance_lu)
                    .build(),
                glib::ParamSpecDouble::builder("max-gain-reduction-db")
                    .default_value(AutomixEngineConfig::default().max_gain_reduction_db)
                    .build(),
                glib::ParamSpecDouble::builder("attack-seconds")
                    .default_value(GainComputerConfig::default().attack_seconds)
                    .build(),
                glib::ParamSpecDouble::builder("hold-seconds")
                    .default_value(GainComputerConfig::default().hold_seconds)
                    .build(),
                glib::ParamSpecDouble::builder("release-seconds")
                    .default_value(GainComputerConfig::default().release_seconds)
                    .build(),
                glib::ParamSpecBoolean::builder("automix-enable")
                    .default_value(true)
                    .build(),
                glib::ParamSpecDouble::builder("current-gain-reduction-db")
                    .default_value(0.0)
                    .read_only()
                    .build(),
                glib::ParamSpecDouble::builder("current-ratio-lu")
                    .default_value(0.0)
                    .read_only()
                    .build(),
            ]
        })
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "target-ratio" => lock_recover(&self.settings).target_ratio_lu.to_value(),
            "max-tolerance" => lock_recover(&self.settings).max_tolerance_lu.to_value(),
            "min-tolerance" => lock_recover(&self.settings).min_tolerance_lu.to_value(),
            "max-gain-reduction-db" => lock_recover(&self.settings).max_gain_reduction_db.to_value(),
            "attack-seconds" => lock_recover(&self.settings).attack_seconds.to_value(),
            "hold-seconds" => lock_recover(&self.settings).hold_seconds.to_value(),
            "release-seconds" => lock_recover(&self.settings).release_seconds.to_value(),
            "automix-enable" => lock_recover(&self.settings).automix_enabled.to_value(),
            "current-gain-reduction-db" => lock_recover(&self.state).processor.applied_gain_reduction_db().to_value(),
            "current-ratio-lu" => {
                // Cheap diagnostic read; not stored separately from the engine's own state.
                0.0f64.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        // Settings changes only take effect for state rebuilt from them; since M1-era engines
        // (RatioEngine/AutomixEngine) don't yet support live-reconfiguration, this rebuilds the
        // whole ProcessingState (losing accumulated loudness history and any in-flight mix
        // alignment) - acceptable for this first pass, worth revisiting if live property changes
        // need to preserve state.
        let mut settings = lock_recover(&self.settings);
        match pspec.name() {
            "target-ratio" => settings.target_ratio_lu = value.get().unwrap(),
            "max-tolerance" => settings.max_tolerance_lu = value.get().unwrap(),
            "min-tolerance" => settings.min_tolerance_lu = value.get().unwrap(),
            "max-gain-reduction-db" => settings.max_gain_reduction_db = value.get().unwrap(),
            "attack-seconds" => settings.attack_seconds = value.get().unwrap(),
            "hold-seconds" => settings.hold_seconds = value.get().unwrap(),
            "release-seconds" => settings.release_seconds = value.get().unwrap(),
            "automix-enable" => {
                settings.automix_enabled = value.get().unwrap();
                return;
            }
            _ => unimplemented!(),
        }
        *lock_recover(&self.state) = ProcessingState::new(&settings);
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
            let bed_caps = audio_caps(BED_CHANNELS as i32);
            let dialogue_caps = audio_caps(1);
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

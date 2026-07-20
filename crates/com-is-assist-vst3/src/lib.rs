// Com-IS-Assist's VST3 plugin: same shared `com_is_assist_core::automix::AutomixProcessor` as the GStreamer
// element, wrapped in a single fixed 8-channel-in/8-channel-out bus instead of the GStreamer
// element's 2-sink/3-src multi-pad layout.
//
// Channel layout (fixed, both input and output): 0-5 = Bed (6ch, Left/Right/Center/LFE/Ls/Rs), 6 =
// Dialogue (mono), 7 = unused (silent). A single fixed bus (rather than nih-plug's named multi-bus
// model, which `Specs/Ressouces/vstDefinitions.md` had flagged as an open question - `nih-plug`'s
// bus model is "one main input + one main output, plus anonymous aux ports," which doesn't map
// cleanly onto this project's originally-planned 2-named-input/3-named-output layout) sidesteps
// that uncertainty entirely and avoids the multi-output-bus host-compatibility risk multi-bus VST3
// plugins can have.
//
// `mix-dialogue` (the "bus insert mode" toggle): when on, output channels 0-2 (Left/Right/Center)
// carry leveled Bed + Dialogue mixed in via `mix_dialogue_into_bed`'s divergence-based pan law
// (matches the GStreamer element's `mix_src`, restricted to L/R/C - Dialogue never touches LFE or
// surrounds); when off, output channels 0-5 are leveled Bed alone (matches `is_leveled_src` - for
// inserting directly on a Bed/IS channel strip where the console sums in commentary separately
// downstream). Channel 6 always carries the dry Dialogue passthrough regardless of this toggle.

use com_is_assist_core::automix::{mix_dialogue_into_bed, AutomixEngineConfig, AutomixProcessor, GainComputerConfig};
use nih_plug::prelude::*;
use std::num::NonZeroU32;
use std::sync::Arc;

const BED_CHANNELS: u32 = 6;
const TOTAL_CHANNELS: usize = 8; // 6 Bed + 1 Dialogue + 1 unused
const DIALOGUE_CHANNEL: usize = BED_CHANNELS as usize; // index 6 (the 7th channel)
const UNUSED_CHANNEL: usize = TOTAL_CHANNELS - 1; // index 7 (the 8th channel)
// Bed channel indices, per `Ebur128Meter::bed_channel_map`'s 6-channel layout: Left, Right,
// Center, Unused (LFE), LeftSurround, RightSurround.
const LEFT_CHANNEL: usize = 0;
const RIGHT_CHANNEL: usize = 1;
const CENTER_CHANNEL: usize = 2;
const TICK_SECONDS: f64 = 0.1;
const MIN_TIME_MS: f32 = 1.0;
const MAX_TIME_MS: f32 = 5000.0;

struct ComISAssist {
    params: Arc<ComISAssistParams>,
    /// `None` until `initialize()` supplies the host's sample rate - `AutomixProcessor` needs it
    /// at construction time (for the `ebur128` meters), which isn't known any earlier than that.
    processor: Option<AutomixProcessor>,
}

#[derive(Params)]
struct ComISAssistParams {
    #[id = "target-ratio"]
    pub target_ratio: FloatParam,
    #[id = "max-gain-reduction-db"]
    pub max_gain_reduction_db: FloatParam,
    /// Milliseconds, not seconds - see the module doc comment and `MIN_TIME_MS`/`MAX_TIME_MS`.
    #[id = "attack-ms"]
    pub attack_ms: FloatParam,
    #[id = "hold-ms"]
    pub hold_ms: FloatParam,
    #[id = "release-ms"]
    pub release_ms: FloatParam,
    #[id = "automix-enable"]
    pub automix_enable: BoolParam,
    /// The "bus insert mode" toggle - see the module doc comment.
    #[id = "mix-dialogue"]
    pub mix_dialogue: BoolParam,
    /// "Voice divergence" (`Specs/Ressouces/UI.md`): 0% = Dialogue is Center-only when mixed in,
    /// 100% = split across Left/Right only. See `mix_dialogue_into_bed`'s doc comment for the pan
    /// law. Only has an effect while `mix_dialogue` is on.
    #[id = "divergence"]
    pub divergence: FloatParam,

    // Live gain-reduction/ratio meters are NOT exposed as parameters here - nih-plug's parameter
    // setter trait (`ParamMut`) is deliberately `pub(crate)`, so a plugin cannot programmatically
    // update its own parameter's displayed value from `process()`; live meters are only supported
    // via a custom GUI reading shared atomic state directly, which conflicts with this project's
    // "no custom GUI, parameters only" decision (see Specs/TechnicalConcept.md section 11's
    // non-goal). Flagged to the user rather than silently faked - see the conversation this was
    // discovered in for the open question on how to proceed.
}

impl Default for ComISAssist {
    fn default() -> Self {
        Self {
            params: Arc::new(ComISAssistParams::default()),
            processor: None,
        }
    }
}

impl Default for ComISAssistParams {
    fn default() -> Self {
        let automix = AutomixEngineConfig::default();
        let gain = GainComputerConfig::default();
        Self {
            target_ratio: FloatParam::new(
                "Target Ratio",
                automix.target_ratio_lu as f32,
                FloatRange::Linear { min: -12.0, max: 12.0 },
            )
            .with_unit(" LU"),
            max_gain_reduction_db: FloatParam::new(
                "Max Gain Reduction",
                automix.max_gain_reduction_db as f32,
                FloatRange::Linear { min: 0.0, max: 48.0 },
            )
            .with_unit(" dB"),
            attack_ms: FloatParam::new(
                "Fade Down Time",
                (gain.attack_seconds * 1000.0) as f32,
                FloatRange::Skewed {
                    min: MIN_TIME_MS,
                    max: MAX_TIME_MS,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_step_size(1.0)
            .with_unit(" ms"),
            hold_ms: FloatParam::new(
                "Hold Time",
                (gain.hold_seconds * 1000.0) as f32,
                FloatRange::Skewed {
                    min: MIN_TIME_MS,
                    max: MAX_TIME_MS,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_step_size(1.0)
            .with_unit(" ms"),
            release_ms: FloatParam::new(
                "Recovery Time",
                (gain.release_seconds * 1000.0) as f32,
                FloatRange::Skewed {
                    min: MIN_TIME_MS,
                    max: MAX_TIME_MS,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_step_size(1.0)
            .with_unit(" ms"),
            automix_enable: BoolParam::new("Automix Enable", true),
            mix_dialogue: BoolParam::new("Mix Dialogue", true),
            divergence: FloatParam::new("Voice Divergence", 0.0, FloatRange::Linear { min: 0.0, max: 100.0 })
                .with_unit(" %"),
        }
    }
}

impl ComISAssistParams {
    fn automix_config(&self) -> AutomixEngineConfig {
        AutomixEngineConfig {
            target_ratio_lu: self.target_ratio.value() as f64,
            max_gain_reduction_db: self.max_gain_reduction_db.value() as f64,
            ..AutomixEngineConfig::default()
        }
    }

    fn gain_computer_config(&self) -> GainComputerConfig {
        GainComputerConfig {
            attack_seconds: self.attack_ms.value() as f64 / 1000.0,
            hold_seconds: self.hold_ms.value() as f64 / 1000.0,
            release_seconds: self.release_ms.value() as f64 / 1000.0,
            max_rate_db_per_s: None,
        }
    }
}

impl Plugin for ComISAssist {
    const NAME: &'static str = "Com-IS-Assist";
    const VENDOR: &'static str = "Com-IS-Assist";
    const URL: &'static str = "https://github.com/com-is-assist/com-is-assist";
    const EMAIL: &'static str = "info@example.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // Single fixed 8-in/8-out bus - see the module doc comment for why this replaces the
    // originally-planned named multi-bus layout.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(TOTAL_CHANNELS as u32),
        main_output_channels: NonZeroU32::new(TOTAL_CHANNELS as u32),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.processor = AutomixProcessor::new(
            BED_CHANNELS,
            buffer_config.sample_rate as u32,
            self.params.automix_config(),
            self.params.gain_computer_config(),
            TICK_SECONDS,
        )
        .ok();
        self.processor.is_some()
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let Some(processor) = self.processor.as_mut() else {
            return ProcessStatus::Error("processor not initialized");
        };

        let automix_enabled = self.params.automix_enable.value();
        let mix_dialogue = self.params.mix_dialogue.value();
        let divergence = self.params.divergence.value() / 100.0;

        let num_samples = buffer.samples();
        let channels = buffer.as_slice();
        if channels.len() < TOTAL_CHANNELS {
            return ProcessStatus::Error("expected a fixed 8-channel bus");
        }

        // com-is-assist-core works with interleaved audio (matching the GStreamer element's convention);
        // nih-plug hands us planar per-channel slices, so de-interleave Bed for the shared
        // processor and re-interleave on the way back out.
        let mut interleaved_bed = vec![0.0f32; num_samples * BED_CHANNELS as usize];
        for frame in 0..num_samples {
            for ch in 0..BED_CHANNELS as usize {
                interleaved_bed[frame * BED_CHANNELS as usize + ch] = channels[ch][frame];
            }
        }
        let dialogue: Vec<f32> = channels[DIALOGUE_CHANNEL][..num_samples].to_vec();

        processor.apply_gain_to_bed_chunk(&mut interleaved_bed);
        // Closed loop (Specs/TechnicalConcept.md section 4): the Bed meter observes the
        // already-gained signal, not the dry one. A mismatched channel count here would be a
        // caller bug in this fixed-layout plugin, not a runtime condition - deliberately ignored
        // rather than crashing the audio thread over it.
        let _ = processor.feed_bed(&interleaved_bed);
        let _ = processor.feed_dialogue(&dialogue);
        processor.maybe_run_control_step(automix_enabled);

        if mix_dialogue {
            mix_dialogue_into_bed(
                &mut interleaved_bed,
                &dialogue,
                BED_CHANNELS,
                LEFT_CHANNEL,
                RIGHT_CHANNEL,
                CENTER_CHANNEL,
                divergence,
            );
        }

        for frame in 0..num_samples {
            for ch in 0..BED_CHANNELS as usize {
                channels[ch][frame] = interleaved_bed[frame * BED_CHANNELS as usize + ch];
            }
            channels[DIALOGUE_CHANNEL][frame] = dialogue[frame];
        }
        // Channel 8 (index 7) is always unused/silent, regardless of what the host sent in.
        channels[UNUSED_CHANNEL][..num_samples].fill(0.0);

        ProcessStatus::Normal
    }
}

impl Vst3Plugin for ComISAssist {
    const VST3_CLASS_ID: [u8; 16] = *b"ComISAssistDuck1";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx, Vst3SubCategory::Dynamics];
}

nih_export_vst3!(ComISAssist);

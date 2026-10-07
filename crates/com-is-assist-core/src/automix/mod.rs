mod automix_engine;
mod dialogue_mix;
mod envelope_detector;
mod fast_loudness;
mod gain_ramp;
mod loop_bank;
mod mix_state;
mod processor;

pub use automix_engine::{AutomixEngine, AutomixEngineConfig, AutomixResult, LoudnessSnapshot};
pub use fast_loudness::FastLoudnessMeter;
pub use loop_bank::{LoopBank, LoopBankConfig, LoopContributions};
pub use mix_state::MixState;
pub use dialogue_mix::{bed_lrc_channels, mix_dialogue_into_bed};
pub use envelope_detector::EnvelopeDetector;
pub use gain_ramp::{apply_ramped_gain, apply_ramped_gain_at, ramp_value_at};
pub use processor::AutomixProcessor;

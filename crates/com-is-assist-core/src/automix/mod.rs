mod automix_engine;
mod dialogue_mix;
mod envelope_detector;
mod fast_loudness;
mod gain_ramp;
mod loop_bank;
mod mix_state;
mod processor;

pub use automix_engine::{
    AutomixEngine, AutomixEngineConfig, AutomixResult, DuckDepth, LoudnessSnapshot, MAX_BED_CHANNELS,
};
pub use fast_loudness::FastLoudnessMeter;
pub use loop_bank::{LoopBank, LoopBankConfig, LoopContributions};
pub use mix_state::MixState;
pub use dialogue_mix::mix_dialogue_into_bed;
pub use envelope_detector::EnvelopeDetector;
pub use gain_ramp::apply_ramped_gain_per_channel;
pub use processor::AutomixProcessor;

mod automix_engine;
mod dialogue_mix;
mod envelope_detector;
mod k_weighted_loudness;
mod gain_ramp;
mod loop_bank;
mod mix_state;
mod processor;

pub use automix_engine::{
    AutomixEngine, AutomixEngineConfig, AutomixResult, DuckDepth, LoudnessSnapshot, MAX_BED_CHANNELS,
};
pub use k_weighted_loudness::KWeightedLoudnessMeter;
pub use loop_bank::{LoopBank, LoopBankConfig, LoopContributions};
pub use mix_state::MixState;
pub use dialogue_mix::mix_dialogue_into_bed;
pub use envelope_detector::EnvelopeDetector;
pub use gain_ramp::apply_ramped_gain_per_channel;
pub use processor::AutomixProcessor;

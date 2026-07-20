mod automix_engine;
mod dialogue_mix;
mod envelope_detector;
mod gain_computer;
mod gain_ramp;
mod processor;

pub use automix_engine::{AutomixEngine, AutomixEngineConfig, AutomixResult};
pub use dialogue_mix::mix_dialogue_into_bed;
pub use envelope_detector::{EnvelopeDetector, EnvelopeDetectorConfig};
pub use gain_computer::{GainComputer, GainComputerConfig};
pub use gain_ramp::{apply_ramped_gain, apply_ramped_gain_at, ramp_value_at};
pub use processor::AutomixProcessor;

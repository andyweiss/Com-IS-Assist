use ort::session::Session;
use ort::value::Tensor;
use rubato::{FftFixedIn, Resampler};
use std::fmt;

/// Vendored directly into the binary (not read from disk at runtime) - see
/// `crates/com-is-assist-core/assets/silero_vad.onnx` and `docs/LICENSING.md`'s "Third-party
/// reference material" section for provenance/license (MIT, snakers4/silero-vad). Embedding it
/// means a GStreamer plugin or VST3 bundle never has to locate/ship a separate data file, or care
/// what the process's working directory happens to be at load time.
const SILERO_VAD_MODEL_BYTES: &[u8] = include_bytes!("../assets/silero_vad.onnx");

/// Silero's ONNX graph operates at 16kHz (or 8kHz) regardless of the project's own sample rate -
/// audio is resampled down to this before every inference.
const VAD_SAMPLE_RATE: usize = 16_000;
/// One inference covers exactly this many (resampled) samples - 32ms at 16kHz. Not a tunable: it's
/// baked into the exported graph's expected input shape.
const VAD_CHUNK_SAMPLES: usize = 512;
/// The model also expects the *previous* chunk's last 64 (resampled) samples prepended as
/// context - see `SileroVad::run_one_chunk`'s doc comment.
const VAD_CONTEXT_SAMPLES: usize = 64;
/// The model's recurrent state tensor is shape `[2, 1, 128]` (batch size fixed at 1 here) -
/// flattened, that's 256 `f32`s, zero-initialized and threaded through every call.
const VAD_STATE_LEN: usize = 2 * 128;

/// How many native-rate frames are pulled through the resampler per call. Not tied to
/// `VAD_CHUNK_SAMPLES` in any exact way (resampled output is buffered and drained in
/// `VAD_CHUNK_SAMPLES`-sized pieces regardless of how much a given resampler call produces) -
/// chosen as a round ~100ms at the native rate, matching this project's own control-tick
/// granularity, purely so the resampler isn't invoked on tiny buffers when a wrapper feeds audio
/// in small blocks.
fn native_chunk_frames(sample_rate: u32) -> usize {
    (sample_rate as usize / 10).max(1)
}

#[derive(Debug)]
pub enum VoiceActivityError {
    Ort(ort::Error),
    ResamplerConstruction(rubato::ResamplerConstructionError),
    Resample(rubato::ResampleError),
}

impl fmt::Display for VoiceActivityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VoiceActivityError::Ort(e) => write!(f, "ONNX Runtime error: {e}"),
            VoiceActivityError::ResamplerConstruction(e) => write!(f, "resampler construction error: {e}"),
            VoiceActivityError::Resample(e) => write!(f, "resample error: {e}"),
        }
    }
}

impl std::error::Error for VoiceActivityError {}

impl From<ort::Error> for VoiceActivityError {
    fn from(e: ort::Error) -> Self {
        VoiceActivityError::Ort(e)
    }
}

impl From<rubato::ResamplerConstructionError> for VoiceActivityError {
    fn from(e: rubato::ResamplerConstructionError) -> Self {
        VoiceActivityError::ResamplerConstruction(e)
    }
}

impl From<rubato::ResampleError> for VoiceActivityError {
    fn from(e: rubato::ResampleError) -> Self {
        VoiceActivityError::Resample(e)
    }
}

/// Tunes how raw per-chunk speech probabilities turn into a stable `voice_active` boolean.
#[derive(Debug, Clone, Copy)]
pub struct VoiceActivityConfig {
    /// A chunk counts as "speech" once its probability crosses this. Silero's own examples
    /// default to 0.5.
    pub speech_probability_threshold: f32,
    /// How long `voice_active` keeps reporting `true` after the last chunk that crossed the
    /// threshold, before dropping back to `false` - without this, brief within-word dips below
    /// threshold would chatter the signal on and off every 32ms, which is exactly the kind of
    /// flutter `Specs/TechnicalConcept.md` section 5/6 already had to solve once for the
    /// LUFS-floor stand-in this replaces.
    ///
    /// This is effectively "how long a pause still counts as *still talking*", so it has to span a
    /// natural inter-sentence break, not just an inter-word one. It directly gates the automix
    /// state machine (`automix::MixState`), and measured commentary pauses run ~1.2s: at the
    /// original 300ms this dropped to `ReleaseToUnity` in every one of them, shedding several dB
    /// of reduction and then re-ducking on the next sentence - an audible swell repeating at
    /// sentence rate. See `Specs/TechnicalConcept.md` section 5's pumping analysis.
    pub hangover_seconds: f64,
}

impl Default for VoiceActivityConfig {
    fn default() -> Self {
        Self {
            speech_probability_threshold: 0.5,
            hangover_seconds: 0.8,
        }
    }
}

/// Real voice-activity detection on a mono audio stream, via Silero VAD (a small recurrent ONNX
/// model) run through `ort`. Replaces the LUFS-floor "is COM currently silent?" stand-in
/// the automix control loop used before this existed (`Specs/TechnicalConcept.md` section 5 always
/// flagged that as temporary) - a loud non-speech noise (room tone, static, another open mic) no longer
/// gets treated as "the target dialogue is present," which the LUFS floor couldn't distinguish.
pub struct SileroVad {
    session: Session,

    resampler: FftFixedIn<f32>,
    native_chunk_frames: usize,
    native_sample_rate: f64,
    /// Native-rate samples not yet handed to the resampler (fewer than `native_chunk_frames`).
    raw_pending: Vec<f32>,
    /// Resampled (16kHz) samples not yet consumed by a `VAD_CHUNK_SAMPLES`-sized inference.
    resampled_pending: Vec<f32>,

    state: Vec<f32>,
    /// The last `VAD_CONTEXT_SAMPLES` resampled samples from the previous inference, prepended to
    /// the next one - see `run_one_chunk`'s doc comment.
    context: Vec<f32>,

    config: VoiceActivityConfig,
    hangover_remaining_seconds: f64,
    voice_active: bool,
    last_speech_probability: f32,
}

impl SileroVad {
    /// `sample_rate` is the native rate of the audio that will be passed to `feed` - not
    /// necessarily 48kHz; VST3 hosts can run at other rates.
    pub fn new(sample_rate: u32, config: VoiceActivityConfig) -> Result<Self, VoiceActivityError> {
        let session = Session::builder()?.commit_from_memory(SILERO_VAD_MODEL_BYTES)?;

        let native_chunk_frames = native_chunk_frames(sample_rate);
        let resampler = FftFixedIn::<f32>::new(sample_rate as usize, VAD_SAMPLE_RATE, native_chunk_frames, 1, 1)?;

        Ok(Self {
            session,
            resampler,
            native_chunk_frames,
            native_sample_rate: sample_rate as f64,
            raw_pending: Vec::new(),
            resampled_pending: Vec::new(),
            state: vec![0.0; VAD_STATE_LEN],
            context: vec![0.0; VAD_CONTEXT_SAMPLES],
            config,
            hangover_remaining_seconds: 0.0,
            voice_active: false,
            last_speech_probability: 0.0,
        })
    }

    /// Feeds one chunk of mono, native-sample-rate dialogue audio (any size). Resamples to 16kHz,
    /// runs as many complete 32ms Silero inferences as the newly-arrived audio allows, and updates
    /// `voice_active`/`speech_probability` accordingly. Safe to call with whatever block size the
    /// caller naturally has - internal buffering handles the rest.
    pub fn feed(&mut self, samples: &[f32]) -> Result<(), VoiceActivityError> {
        self.hangover_remaining_seconds -= samples.len() as f64 / self.native_sample_rate;

        self.raw_pending.extend_from_slice(samples);
        while self.raw_pending.len() >= self.native_chunk_frames {
            let native_chunk: Vec<f32> = self.raw_pending.drain(..self.native_chunk_frames).collect();
            let resampled = self.resampler.process(&[native_chunk], None)?;
            self.resampled_pending.extend_from_slice(&resampled[0]);
        }

        while self.resampled_pending.len() >= VAD_CHUNK_SAMPLES {
            let chunk: Vec<f32> = self.resampled_pending.drain(..VAD_CHUNK_SAMPLES).collect();
            let probability = self.run_one_chunk(&chunk)?;
            self.last_speech_probability = probability;
            if probability >= self.config.speech_probability_threshold {
                self.hangover_remaining_seconds = self.config.hangover_seconds;
            }
        }

        self.voice_active = self.hangover_remaining_seconds > 0.0;
        Ok(())
    }

    /// Runs one 32ms chunk (already resampled to 16kHz, exactly `VAD_CHUNK_SAMPLES` long) through
    /// the model, returning its raw speech probability (not yet thresholded/hangover-smoothed -
    /// see `feed`). The model expects the previous chunk's last `VAD_CONTEXT_SAMPLES` samples
    /// prepended to this one (`self.context`), and carries a recurrent `state` tensor forward
    /// between calls - both threaded through exactly as Silero's own reference Python wrapper does
    /// (`OnnxWrapper.__call__` in the upstream repo).
    fn run_one_chunk(&mut self, chunk: &[f32]) -> Result<f32, VoiceActivityError> {
        let mut model_input = Vec::with_capacity(VAD_CONTEXT_SAMPLES + VAD_CHUNK_SAMPLES);
        model_input.extend_from_slice(&self.context);
        model_input.extend_from_slice(chunk);

        let input_tensor = Tensor::from_array(([1usize, model_input.len()], model_input.clone()))?;
        let state_tensor = Tensor::from_array(([2usize, 1, 128], self.state.clone()))?;
        let sr_tensor = Tensor::from_array(([1usize], vec![VAD_SAMPLE_RATE as i64]))?;

        let outputs = self.session.run(ort::inputs![
            "input" => input_tensor,
            "state" => state_tensor,
            "sr" => sr_tensor,
        ])?;

        let (_, probability) = outputs[0].try_extract_tensor::<f32>()?;
        let (_, new_state) = outputs[1].try_extract_tensor::<f32>()?;
        self.state.copy_from_slice(new_state);
        self.context.copy_from_slice(&model_input[model_input.len() - VAD_CONTEXT_SAMPLES..]);

        Ok(probability[0])
    }

    pub fn voice_active(&self) -> bool {
        self.voice_active
    }

    pub fn speech_probability(&self) -> f32 {
        self.last_speech_probability
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_vendored_model_and_reports_silence_as_inactive() {
        let mut vad = SileroVad::new(48_000, VoiceActivityConfig::default()).expect("model should load and construct");

        // Feed a full second of digital silence - comfortably more than one hangover window
        // (300ms default) - real speech-detection accuracy needs real audio to validate, but
        // "silence never registers as active" is a baseline any working VAD must satisfy.
        vad.feed(&vec![0.0_f32; 48_000]).expect("feed should succeed");

        assert!(!vad.voice_active(), "pure silence should never register as voice activity");
        assert!(
            vad.speech_probability() < 0.5,
            "expected a low speech probability for silence, got {}",
            vad.speech_probability()
        );
    }

    #[test]
    fn handles_odd_sized_feed_chunks_without_erroring() {
        // Real wrappers won't hand over conveniently-sized blocks - GStreamer buffers and VST3
        // host blocks are both arbitrary sizes. Exercises the internal raw/resampled buffering
        // logic across many small, oddly-sized calls rather than one large aligned one.
        let mut vad = SileroVad::new(44_100, VoiceActivityConfig::default()).expect("model should load and construct");
        for _ in 0..200 {
            vad.feed(&vec![0.0_f32; 137]).expect("feed should succeed");
        }
    }
}

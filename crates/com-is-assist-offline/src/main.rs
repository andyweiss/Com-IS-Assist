// Offline validation CLI (see Specs/TechnicalConcept.md): feeds a Bed + Dialogue WAV pair through
// the shared `AutomixProcessor` - the same multi-loop, feed-forward DSP both plugin wrappers use -
// and prints per-tick measurements as CSV, including what each loop stage is contributing.
// Optionally renders leveled-Bed/Mix WAVs so the result can actually be listened to.
//
// Every tunable is overridable from the environment (see `env_f64`) so a settings sweep can be
// scripted against real material without recompiling.

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor, MixState};
use com_is_assist_core::voice_activity::{SileroVad, VoiceActivityConfig};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::env;
use std::fs;
use std::process::ExitCode;

/// 100ms control tick, matching BS.1770's own gating-block granularity - the R128 readings do not
/// update faster than this, so there is nothing to gain from a shorter one. (The fast loop runs far
/// more often than this, inside `AutomixProcessor`.)
const TICK_SECONDS: f64 = 0.1;

/// Reads one interleaved tick's worth of frames as normalized f32, whatever the file's format.
fn read_tick(
    reader: &mut WavReader<std::io::BufReader<std::fs::File>>,
    frame_count: usize,
    channels: u32,
    format: SampleFormat,
    bits_per_sample: u16,
) -> Vec<f32> {
    let want = frame_count * channels as usize;
    let mut out = Vec::with_capacity(want);
    match format {
        SampleFormat::Float => {
            for sample in reader.samples::<f32>().take(want) {
                match sample {
                    Ok(s) => out.push(s),
                    Err(_) => break,
                }
            }
        }
        SampleFormat::Int => {
            let scale = (1u32 << (bits_per_sample - 1)) as f32;
            for sample in reader.samples::<i32>().take(want) {
                match sample {
                    Ok(s) => out.push(s as f32 / scale),
                    Err(_) => break,
                }
            }
        }
    }
    out
}

/// Reads one `f64` override from the environment, falling back to `default`. An unparseable value
/// is reported and ignored rather than silently treated as the default.
fn env_f64(name: &str, default: f64) -> f64 {
    match env::var(name) {
        Ok(raw) => match raw.parse() {
            Ok(value) => value,
            Err(_) => {
                eprintln!("ignoring {name}={raw:?} (not a number), using {default}");
                default
            }
        },
        Err(_) => default,
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    match env::var(name) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => {
                eprintln!("ignoring {name}={other:?} (not a boolean), using {default}");
                default
            }
        },
        Err(_) => default,
    }
}

/// Averages an interleaved multichannel block to mono for the IS-side voice detector (Silero is a
/// mono model). Every wrapper does the same reduction so the detector sees identical input.
fn downmix_to_mono(interleaved: &[f32], channels: u32) -> Vec<f32> {
    let channels = channels as usize;
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Short, stable CSV labels for the automix state.
fn mix_state_label(state: MixState) -> &'static str {
    match state {
        MixState::ReleaseToUnity => "release",
        MixState::DuckToTarget => "duck_target",
        MixState::InterviewPassthrough => "interview",
        MixState::DuckToOvervoice => "duck_overvoice",
    }
}

fn float_wav_spec(channels: u16, sample_rate: u32) -> WavSpec {
    WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 && args.len() != 4 {
        eprintln!("usage: com-is-assist-offline <bed.wav> <dialogue.wav> [render_output_dir]");
        eprintln!();
        eprintln!("Config overrides (environment variables, all optional):");
        eprintln!("  COMIS_TARGET_RATIO_LU, COMIS_OVERVOICE_RATIO_LU, COMIS_MAX_GAIN_REDUCTION_DB,");
        eprintln!("  COMIS_SPEED, COMIS_LOOKAHEAD_MS, COMIS_INTERVIEW_PASSTHROUGH,");
        eprintln!("  COMIS_VAD_HANGOVER_SECONDS");
        return ExitCode::FAILURE;
    }

    let mut bed_reader = match WavReader::open(&args[1]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("failed to open bed file: {}: {}", args[1], e);
            return ExitCode::FAILURE;
        }
    };
    let mut dialogue_reader = match WavReader::open(&args[2]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("failed to open dialogue file: {}: {}", args[2], e);
            return ExitCode::FAILURE;
        }
    };

    let bed_spec = bed_reader.spec();
    let dialogue_spec = dialogue_reader.spec();

    if bed_spec.sample_rate != dialogue_spec.sample_rate {
        eprintln!(
            "bed and dialogue sample rates must match ({} vs {})",
            bed_spec.sample_rate, dialogue_spec.sample_rate
        );
        return ExitCode::FAILURE;
    }
    if dialogue_spec.channels != 1 {
        eprintln!("dialogue file must be mono (got {} channels)", dialogue_spec.channels);
        return ExitCode::FAILURE;
    }

    let sample_rate = bed_spec.sample_rate;
    let bed_channels = bed_spec.channels as u32;

    let defaults = AutomixEngineConfig::default();
    let config = AutomixEngineConfig {
        target_ratio_lu: env_f64("COMIS_TARGET_RATIO_LU", defaults.target_ratio_lu),
        overvoice_ratio_lu: env_f64("COMIS_OVERVOICE_RATIO_LU", defaults.overvoice_ratio_lu),
        max_gain_reduction_db: env_f64("COMIS_MAX_GAIN_REDUCTION_DB", defaults.max_gain_reduction_db),
        speed: env_f64("COMIS_SPEED", defaults.speed),
        interview_passthrough_enabled: env_bool("COMIS_INTERVIEW_PASSTHROUGH", defaults.interview_passthrough_enabled),
    };

    let mut processor = match AutomixProcessor::new(bed_channels, sample_rate, config, TICK_SECONDS) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("unsupported bed channel count {bed_channels}: {e:?}");
            return ExitCode::FAILURE;
        }
    };
    processor.set_lookahead_seconds(env_f64("COMIS_LOOKAHEAD_MS", 0.0) / 1000.0);

    // Real voice-activity detection on both sides - together they select the automix state.
    // COM fails open (always voiced), IS fails closed, matching both plugin wrappers.
    let vad_config = VoiceActivityConfig {
        hangover_seconds: env_f64("COMIS_VAD_HANGOVER_SECONDS", VoiceActivityConfig::default().hangover_seconds),
        ..VoiceActivityConfig::default()
    };
    let mut com_vad = match SileroVad::new(sample_rate, vad_config) {
        Ok(vad) => Some(vad),
        Err(e) => {
            eprintln!("COM voice-activity detection unavailable, failing open: {e}");
            None
        }
    };
    let mut is_vad = SileroVad::new(sample_rate, vad_config).ok();

    let tick_frames = (sample_rate as f64 * TICK_SECONDS) as usize;

    let mut render = args.get(3).map(|dir| {
        fs::create_dir_all(dir).expect("create render output dir");
        let leveled_bed = WavWriter::create(
            format!("{dir}/leveled_bed.wav"),
            float_wav_spec(bed_channels as u16, sample_rate),
        )
        .expect("create leveled_bed.wav");
        let mix = WavWriter::create(
            format!("{dir}/mix.wav"),
            float_wav_spec(bed_channels as u16, sample_rate),
        )
        .expect("create mix.wav");
        (leveled_bed, mix)
    });

    println!(
        "tick,time_s,bed_lufs_m,com_lufs_m,com_voice_active,is_voice_active,mix_state,\
ratio_lu,target_lu,r_slow,r_mid,r_fast,gain_reduction_db"
    );

    let mut tick: u64 = 0;
    loop {
        let bed_block = read_tick(
            &mut bed_reader,
            tick_frames,
            bed_channels,
            bed_spec.sample_format,
            bed_spec.bits_per_sample,
        );
        let dialogue_block = read_tick(
            &mut dialogue_reader,
            tick_frames,
            1,
            dialogue_spec.sample_format,
            dialogue_spec.bits_per_sample,
        );

        let frames_read = (bed_block.len() / bed_channels as usize).min(dialogue_block.len());
        if frames_read == 0 {
            break;
        }

        let dry_bed = &bed_block[..frames_read * bed_channels as usize];
        let dry_dialogue = &dialogue_block[..frames_read];

        // Voice activity on the dry signals, before anything is applied.
        let com_voice_active = match com_vad.as_mut() {
            Some(vad) => {
                vad.feed(dry_dialogue).expect("feed COM voice-activity detector");
                vad.voice_active()
            }
            None => true,
        };
        let is_voice_active = match is_vad.as_mut() {
            Some(vad) => {
                let mono_bed = downmix_to_mono(dry_bed, bed_channels);
                vad.feed(&mono_bed).expect("feed IS voice-activity detector");
                vad.voice_active()
            }
            None => false,
        };

        processor.feed_dialogue(dry_dialogue).expect("feed dialogue meters");

        let mut leveled_bed = dry_bed.to_vec();
        processor.process_bed(&mut leveled_bed).expect("process bed");

        let mut dialogue_out = dry_dialogue.to_vec();
        processor.delay_dialogue(&mut dialogue_out);

        processor.maybe_run_control_step(com_voice_active, is_voice_active);

        let c = processor.contributions();
        println!(
            "{},{:.2},{:.2},{:.2},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2}",
            tick,
            tick as f64 * TICK_SECONDS,
            processor.bed_momentary_lufs(),
            processor.dialogue_momentary_lufs(),
            if com_voice_active { 1 } else { 0 },
            if is_voice_active { 1 } else { 0 },
            mix_state_label(processor.mix_state()),
            processor.applied_ratio_lu(),
            processor.active_target_ratio_lu(),
            c.slow_db,
            c.mid_db,
            c.fast_db,
            c.total_db,
        );

        if let Some((leveled_bed_writer, mix_writer)) = render.as_mut() {
            for frame in 0..frames_read {
                let dialogue_sample = dialogue_out[frame];
                for channel in 0..bed_channels as usize {
                    let bed_sample = leveled_bed[frame * bed_channels as usize + channel];
                    leveled_bed_writer.write_sample(bed_sample).expect("write leveled_bed sample");
                    mix_writer
                        .write_sample(bed_sample + dialogue_sample)
                        .expect("write mix sample");
                }
            }
        }

        tick += 1;
        if frames_read < tick_frames {
            break; // final partial tick
        }
    }

    if let Some((leveled_bed_writer, mix_writer)) = render {
        leveled_bed_writer.finalize().expect("finalize leveled_bed.wav");
        mix_writer.finalize().expect("finalize mix.wav");
    }

    ExitCode::SUCCESS
}

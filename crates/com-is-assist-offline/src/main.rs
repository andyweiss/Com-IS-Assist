// Offline validation CLI (see Specs/TechnicalConcept.md): feeds a Bed + Dialogue WAV pair through
// the closed-loop automix (Ebur128Meter -> RatioEngine -> AutomixEngine, with the Bed meter
// observing the *already-gained* Bed signal per section 4/5's closed-loop design) and prints
// per-tick loudness/ratio/gain as CSV. Optionally renders leveled-Bed/Mix WAVs so the result can
// actually be listened to (M2's "listening sign-off" exit criterion).

use com_is_assist_core::automix::{apply_ramped_gain, AutomixEngine, AutomixEngineConfig, GainComputerConfig};
use com_is_assist_core::loudness::Ebur128Meter;
use com_is_assist_core::ratio::RatioEngine;
use com_is_assist_core::voice_activity::{SileroVad, VoiceActivityConfig};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::env;
use std::fs;
use std::process::ExitCode;

/// Reads one interleaved tick's worth of frames (`frame_count * channels` samples) as
/// normalized f32, regardless of whether the file is integer PCM or IEEE float.
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

    let bed_channel_map = match Ebur128Meter::bed_channel_map(bed_channels) {
        Ok(map) => map,
        Err(e) => {
            eprintln!("unsupported bed channel count {}: {:?}", bed_channels, e);
            return ExitCode::FAILURE;
        }
    };
    // Measures the Bed signal *after* automix gain is applied (closed loop) — see
    // Specs/TechnicalConcept.md section 4.
    let mut bed_meter = Ebur128Meter::new(&bed_channel_map, sample_rate).expect("bed meter config");
    let mut dialogue_meter =
        Ebur128Meter::new(&Ebur128Meter::dialogue_channel_map(), sample_rate).expect("dialogue meter config");

    let mut ratio_engine = RatioEngine::default();
    // Real voice-activity detection on Dialogue - see `com_is_assist_core::voice_activity`'s doc
    // comment for why this replaced the old LUFS-floor "is COM currently silent?" stand-in. Fails
    // open (always voice-active) if construction fails, matching both wrappers' fallback.
    let mut voice_activity = match SileroVad::new(sample_rate, VoiceActivityConfig::default()) {
        Ok(vad) => Some(vad),
        Err(e) => {
            eprintln!("voice-activity detection unavailable, failing open: {e}");
            None
        }
    };

    // 100ms tick, matching the JSFX's LOUD_METER_UPDATE and RatioEngine/AutomixEngine's cadence.
    let tick_seconds = 0.1;
    let tick_frames = (sample_rate as f64 * tick_seconds) as usize;
    let mut automix_engine = AutomixEngine::new(
        AutomixEngineConfig::default(),
        GainComputerConfig::default(),
        tick_seconds,
    );

    let mut render = args.get(3).map(|dir| {
        fs::create_dir_all(dir).expect("create render output dir");
        let leveled_bed = WavWriter::create(
            format!("{dir}/leveled_bed.wav"),
            float_wav_spec(bed_channels as u16, sample_rate),
        )
        .expect("create leveled_bed.wav");
        // Mix mirrors the Bed's channel layout — Dialogue is summed into every channel (the
        // simplest "all-channels" mix mode; the real product exposes this as `dialogue-mix-mode`,
        // see Specs/Ressouces/GSTdefinitions.md).
        let mix = WavWriter::create(
            format!("{dir}/mix.wav"),
            float_wav_spec(bed_channels as u16, sample_rate),
        )
        .expect("create mix.wav");
        (leveled_bed, mix)
    });

    println!("tick,time_s,bed_lufs_m,bed_lufs_s,com_lufs_m,com_lufs_s,voice_active,ratio_lu,ratio_valid,gain_reduction_db");

    // The gain applied to *this* tick's Bed audio ramps from `ramp_start_gain` (the value the
    // previous tick's ramp ended on) to `applied_gain_linear` (whatever AutomixEngine computed at
    // the end of the *previous* tick — this tick's target). Both start at unity. See the
    // closed-loop note in Specs/TechnicalConcept.md section 4, and `apply_ramped_gain`'s doc
    // comment for why a flat per-tick scalar isn't enough (it clicks at every tick boundary).
    let mut applied_gain_reduction_db = 0.0_f64;
    let mut applied_gain_linear = 1.0_f64;
    let mut ramp_start_gain = 1.0_f64;

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

        let bed_frames_read = bed_block.len() / bed_channels as usize;
        let dialogue_frames_read = dialogue_block.len();
        let frames_read = bed_frames_read.min(dialogue_frames_read);
        if frames_read == 0 {
            break;
        }

        let mut leveled_bed: Vec<f32> = bed_block[..frames_read * bed_channels as usize].to_vec();
        apply_ramped_gain(
            &mut leveled_bed,
            bed_channels,
            ramp_start_gain as f32,
            applied_gain_linear as f32,
        );
        ramp_start_gain = applied_gain_linear;

        bed_meter.push_frames(&leveled_bed).expect("push bed frames");
        dialogue_meter
            .push_frames(&dialogue_block[..frames_read])
            .expect("push dialogue frames");

        let bed_m = bed_meter.momentary_loudness_db();
        let bed_s = bed_meter.short_term_loudness_db();
        let com_m = dialogue_meter.momentary_loudness_db();
        let com_s = dialogue_meter.short_term_loudness_db();

        let voice_active = match voice_activity.as_mut() {
            Some(vad) => {
                vad.feed(&dialogue_block[..frames_read]).expect("feed voice-activity detector");
                vad.voice_active()
            }
            None => true,
        };
        let ratio = ratio_engine.update(bed_s, com_s, voice_active);

        println!(
            "{},{:.2},{:.2},{:.2},{:.2},{:.2},{},{:.2},{},{:.2}",
            tick,
            tick as f64 * tick_seconds,
            bed_m,
            bed_s,
            com_m,
            com_s,
            if voice_active { 1 } else { 0 },
            ratio.ratio_lu,
            if ratio.valid { 1 } else { 0 },
            applied_gain_reduction_db,
        );

        if let Some((leveled_bed_writer, mix_writer)) = render.as_mut() {
            for frame in 0..frames_read {
                let dialogue_sample = dialogue_block[frame];
                for channel in 0..bed_channels as usize {
                    let bed_sample = leveled_bed[frame * bed_channels as usize + channel];
                    leveled_bed_writer.write_sample(bed_sample).expect("write leveled_bed sample");
                    mix_writer
                        .write_sample(bed_sample + dialogue_sample)
                        .expect("write mix sample");
                }
            }
        }

        // Compute the gain for the *next* tick from this tick's (now post-gain) measurement.
        let automix_result = automix_engine.process_tick(ratio);
        applied_gain_reduction_db = automix_result.gain_reduction_db;
        applied_gain_linear = automix_result.gain_linear;

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

//! Measures how busy the gain is when the mix only needs a *small* reduction - the complaint being
//! that the plugin does "quite a lot" even then - and how much of that comes from each stage.
//!
//! `cargo run --release --example subtle_reduction -- <com.wav> <bed.wav>... [--seconds N]
//!     [--start S] [--com-gain dB,dB,...]`
//! `cargo run --release --example subtle_reduction -- --profile <file.wav>...`
//!
//! Every input is a mono WAV; several bed files are interleaved in the order given, so a 5.1 bed
//! split into per-channel files can be passed as six paths. `--com-gain` reruns the material with
//! COM shifted by each amount, which moves the *needed* reduction without changing the material:
//! raising COM by 6 dB lowers the reduction the ratio asks for by 6 dB.
//!
//! The needed reduction is taken to be the slow stage's value - its 3s answer is the programme
//! balance; everything the mid and fast stages add on top is movement around it. Results are
//! bucketed by that value, so "subtle" can be read off directly.
//!
//! COM voice comes from the real detector, through the same guard band the plugin uses; IS voice is
//! held off, so only the ordinary duck/release states are exercised.

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor, MixState};
use com_is_assist_core::voice_activity::{SileroVad, VoiceActivityConfig};

const BLOCK: usize = 64;
/// Upper edges of the needed-reduction buckets, in dB.
const BUCKETS: [f64; 4] = [1.0, 3.0, 6.0, f64::INFINITY];

fn read_mono(path: &str, start: f64, seconds: f64) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "{path}: expected a mono file");
    let rate = spec.sample_rate;
    reader.seek((start * rate as f64) as u32).expect("seek");
    let take = (seconds * rate as f64) as usize;
    let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().take(take).map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            reader.samples::<i32>().take(take).map(|s| s.unwrap() as f32 * scale).collect()
        }
    };
    (samples, rate)
}

fn profile(paths: &[String]) {
    for path in paths {
        let (x, rate) = read_mono(path, 0.0, 1e9);
        let block = (rate / 10) as usize;
        let mut levels: Vec<f64> = x
            .chunks_exact(block)
            .map(|b| 10.0 * ((b.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / block as f64) + 1e-12).log10())
            .collect();
        levels.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| levels[((levels.len() - 1) as f64 * p) as usize];
        let silent = levels.iter().filter(|l| **l < -60.0).count() as f64 / levels.len() as f64;
        println!(
            "{path}\n  {:.0}s  p10 {:6.1}  p50 {:6.1}  p90 {:6.1} dBFS  silent {:3.0}%",
            x.len() as f64 / rate as f64,
            pct(0.1),
            pct(0.5),
            pct(0.9),
            silent * 100.0
        );
    }
}

#[derive(Default, Clone, Copy)]
struct Bucket {
    blocks: u64,
    gr: f64,
    slow: f64,
    mid: f64,
    fast: f64,
    /// Sum of |GR change| between blocks, in dB: how much the gain moves.
    travel: f64,
    /// Sum of (GR - slow)^2, for the RMS movement around the needed reduction.
    deviation_sq: f64,
}

struct Run {
    buckets: [Bucket; BUCKETS.len()],
    ducking_blocks: u64,
    total_blocks: u64,
}

fn run(com: &[f32], bed: &[f32], channels: usize, rate: u32, config: AutomixEngineConfig, vad_flags: &[bool]) -> Run {
    let mut processor =
        AutomixProcessor::new(channels as u32, rate, config, 0.1).expect("bed channel count supported");
    let mut buckets = [Bucket::default(); BUCKETS.len()];
    let (mut ducking_blocks, mut total_blocks) = (0, 0);
    let mut previous_gr = 0.0;
    let mut bed_block = vec![0.0_f32; BLOCK * channels];
    for (i, start) in (0..com.len() - BLOCK).step_by(BLOCK).enumerate() {
        let active = vad_flags[i];
        processor.feed_dialogue(&com[start..start + BLOCK]).expect("dialogue");
        processor.set_voice_activity(active, false);
        bed_block.copy_from_slice(&bed[start * channels..(start + BLOCK) * channels]);
        processor.process_bed(&mut bed_block).expect("bed");
        processor.maybe_run_control_step(active, false);

        let gr = processor.applied_gain_reduction_db();
        let c = processor.contributions();
        total_blocks += 1;
        if processor.mix_state() == MixState::DuckToTarget {
            ducking_blocks += 1;
            let index = BUCKETS.iter().position(|edge| c.slow_db < *edge).unwrap();
            let b = &mut buckets[index];
            b.blocks += 1;
            b.gr += gr;
            b.slow += c.slow_db;
            b.mid += c.mid_db;
            b.fast += c.fast_db;
            b.travel += (gr - previous_gr).abs();
            b.deviation_sq += (gr - c.slow_db).powi(2);
        }
        previous_gr = gr;
    }
    Run { buckets, ducking_blocks, total_blocks }
}

fn report(label: &str, run: &Run, rate: u32) {
    let seconds_per_block = BLOCK as f64 / rate as f64;
    println!(
        "  {label}  (ducking {:.0}% of the time)",
        100.0 * run.ducking_blocks as f64 / run.total_blocks.max(1) as f64
    );
    println!("    needed     time   mean GR   slow    mid   fast   movement  travel");
    let mut lower = 0.0;
    for (edge, b) in BUCKETS.iter().zip(run.buckets) {
        let range = if edge.is_finite() { format!("{lower:.0}-{edge:.0} dB") } else { format!(">{lower:.0} dB") };
        lower = *edge;
        if b.blocks == 0 {
            println!("    {range:8}     -");
            continue;
        }
        let n = b.blocks as f64;
        println!(
            "    {range:8} {:5.0}s  {:6.2}  {:6.2} {:+6.2} {:6.2}   {:5.2} dB  {:5.1} dB/s",
            n * seconds_per_block,
            b.gr / n,
            b.slow / n,
            b.mid / n,
            b.fast / n,
            (b.deviation_sq / n).sqrt(),
            b.travel / (n * seconds_per_block),
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--profile") {
        profile(&args[1..]);
        return;
    }

    let mut files = Vec::new();
    let (mut seconds, mut start) = (600.0, 0.0);
    let mut com_gains = vec![0.0];
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" => seconds = it.next().and_then(|v| v.parse().ok()).expect("--seconds N"),
            "--start" => start = it.next().and_then(|v| v.parse().ok()).expect("--start S"),
            "--com-gain" => {
                com_gains = it
                    .next()
                    .expect("--com-gain dB,dB,...")
                    .split(',')
                    .map(|v| v.trim().parse().expect("--com-gain takes numbers"))
                    .collect()
            }
            _ => files.push(arg),
        }
    }
    assert!(files.len() >= 2, "usage: subtle_reduction <com.wav> <bed.wav>... [--seconds N] [--start S] [--com-gain dB,...]");

    let (com_raw, rate) = read_mono(&files[0], start, seconds);
    let bed_channels: Vec<Vec<f32>> = files[1..]
        .iter()
        .map(|path| {
            let (x, r) = read_mono(path, start, seconds);
            assert_eq!(r, rate, "{path}: sample rate differs from COM");
            x
        })
        .collect();
    let channels = bed_channels.len();
    let n = bed_channels.iter().map(Vec::len).min().unwrap().min(com_raw.len());
    let mut bed = vec![0.0_f32; n * channels];
    for (c, channel) in bed_channels.iter().enumerate() {
        for (f, s) in channel[..n].iter().enumerate() {
            bed[f * channels + c] = *s;
        }
    }
    let com_raw = &com_raw[..n];
    println!("{:.0}s of material, {channels}-channel bed, {rate}Hz\n", n as f64 / rate as f64);

    // The detector is level-independent enough that its answer is computed once, on the unshifted
    // COM, and reused for every gain - so every run compares the same voice decisions.
    let mut vad = SileroVad::new(rate, VoiceActivityConfig::default()).expect("vad");
    let vad_flags: Vec<bool> = (0..n - BLOCK)
        .step_by(BLOCK)
        .map(|start| {
            vad.feed(&com_raw[start..start + BLOCK]).expect("feed");
            vad.voice_active()
        })
        .collect();

    let tolerance = |tolerance_lu| AutomixEngineConfig { tolerance_lu, ..Default::default() };
    let variants = [
        ("tolerance 0 LU (v2.0.0 behaviour)", tolerance(0.0)),
        ("default (tolerance 2 LU)", AutomixEngineConfig::default()),
        ("default, fast stage off", AutomixEngineConfig { fast_stage: false, ..Default::default() }),
        ("tolerance 3 LU", tolerance(3.0)),
    ];
    for gain_db in com_gains {
        let gain = 10f32.powf(gain_db as f32 / 20.0);
        let com: Vec<f32> = com_raw.iter().map(|s| s * gain).collect();
        println!("COM {gain_db:+.0} dB");
        for (label, config) in variants {
            report(label, &run(&com, &bed, channels, rate, config, &vad_flags), rate);
        }
        println!();
    }
    println!("needed   = the slow stage's value (the 3s programme balance), by bucket");
    println!("movement = RMS of (GR - slow): how far the gain strays from the needed reduction");
    println!("travel   = total gain change per second while ducking");
}

//! Measures how long after speech begins the VAD actually reports it.
//!
//! `cargo run --release --example detect_latency -- <mono.wav>`
//!
//! Rather than trying to infer a ground-truth onset from energy - unreliable on real broadcast
//! material, where the gaps are rarely clean - this splices **real speech at a known sample
//! index** after a stretch of digital silence. The answer is then exact: the delay between the
//! splice point and the detector's rising edge, with nothing to argue about.
//!
//! What it measures is the whole chain: the resampler's input chunk, Silero's own 32ms inference
//! hop, and the hangover logic. It does *not* include `BackgroundVad`'s poll interval (add up to
//! that).

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor};
use com_is_assist_core::voice_activity::{SileroVad, VoiceActivityConfig};

const BLOCK: usize = 64;
const SILENCE_SECONDS: f64 = 2.0;
const SPEECH_SECONDS: f64 = 4.0;

fn read_mono(path: &str) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "expects a mono file");
    let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => reader.samples::<i32>().map(|s| s.unwrap() as f32 * scale).collect(),
    };
    (samples, spec.sample_rate)
}

/// Finds starts of sustained loud passages - somewhere the detector will certainly fire, so the
/// measurement is of the pipeline's delay rather than of Silero's willingness to commit.
fn speech_regions(samples: &[f32], rate: u32, want: usize) -> Vec<usize> {
    let win = (0.050 * rate as f64) as usize;
    let needed = (0.600 * rate as f64) as usize / win;
    let threshold = 10f32.powf(-30.0 / 20.0);
    let (mut found, mut run) = (Vec::new(), 0usize);
    let mut i = 0;
    while i + win < samples.len() && found.len() < want {
        let rms = (samples[i..i + win].iter().map(|s| s * s).sum::<f32>() / win as f32).sqrt();
        if rms > threshold {
            run += 1;
            if run == needed {
                found.push(i - (needed - 1) * win);
                i += (3.0 * rate as f64) as usize; // skip ahead so regions are independent
                run = 0;
            }
        } else {
            run = 0;
        }
        i += win;
    }
    found
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: detect_latency <mono.wav>");
    let (samples, rate) = read_mono(&path);
    let regions = speech_regions(&samples, rate, 12);
    assert!(!regions.is_empty(), "found no sustained speech in {path}");
    println!("{path}: {} speech regions sampled\n", regions.len());

    let silence = (SILENCE_SECONDS * rate as f64) as usize;
    let speech = (SPEECH_SECONDS * rate as f64) as usize;
    let mut latencies = Vec::new();

    for &region in &regions {
        if region + speech >= samples.len() {
            continue;
        }
        // [2s digital silence][4s real speech] - the splice point is the exact onset.
        let mut signal = vec![0.0_f32; silence];
        signal.extend_from_slice(&samples[region..region + speech]);

        // A fresh detector each time: the recurrent state and hangover must not carry over.
        let mut vad = SileroVad::new(rate, VoiceActivityConfig::default()).expect("vad");
        for start in (0..signal.len() - BLOCK).step_by(BLOCK) {
            vad.feed(&signal[start..start + BLOCK]).expect("feed");
            if vad.voice_active() {
                if start >= silence {
                    latencies.push((start - silence) as f64 / rate as f64 * 1000.0);
                }
                break;
            }
        }
    }

    report("VAD flag", &mut latencies);

    // End to end: the figure an operator actually hears - speech onset to the bed starting to
    // move. Adds the control path on top of the detector: state adoption, the gain loop's
    // attack, and the gain ramp.
    let mut duck = Vec::new();
    for &region in &regions {
        if region + speech >= samples.len() {
            continue;
        }
        let mut dialogue = vec![0.0_f32; silence];
        dialogue.extend_from_slice(&samples[region..region + speech]);
        // A steady bed, so the only thing that can move the gain is the commentary starting.
        let bed_tone: Vec<f32> = (0..dialogue.len())
            .map(|i| 0.3 * (2.0 * std::f64::consts::PI * 220.0 * i as f64 / rate as f64).sin() as f32)
            .collect();

        let mut vad = SileroVad::new(rate, VoiceActivityConfig::default()).expect("vad");
        let mut processor = AutomixProcessor::new(
            1,
            rate,
            AutomixEngineConfig { com_gate_lufs: -70.0, ..AutomixEngineConfig::default() },
        )
        .expect("processor");

        for start in (0..dialogue.len() - BLOCK).step_by(BLOCK) {
            let d = &dialogue[start..start + BLOCK];
            vad.feed(d).expect("feed");
            let active = vad.voice_active();
            processor.feed_dialogue(d).expect("dialogue");
            processor.set_voice_activity(active, false);
            let mut bed = bed_tone[start..start + BLOCK].to_vec();
            processor.process_bed(&mut bed).expect("bed");
            if processor.applied_gain_reduction_db() > 0.5 && start >= silence {
                duck.push((start - silence) as f64 / rate as f64 * 1000.0);
                break;
            }
        }
    }
    report("bed ducking", &mut duck);
}

fn report(label: &str, values: &mut [f64]) {
    if values.is_empty() {
        println!("{label}: never fired");
        return;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    println!(
        "{label:<12} from speech onset   mean {:.0}ms   median {:.0}ms   min {:.0}ms   max {:.0}ms   (n={})",
        mean,
        values[values.len() / 2],
        values.first().unwrap(),
        values.last().unwrap(),
        values.len()
    );
}

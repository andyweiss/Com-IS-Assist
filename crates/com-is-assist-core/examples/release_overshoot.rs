//! Measures whether the Bed ducks *harder* after the commentator stops - an audible dip at the
//! end of a phrase, where recovery is what should happen.
//!
//! `cargo run --release --example release_overshoot -- <com.wav> <bed.wav> [seconds]`
//!
//! Runs real, time-aligned commentary and bed through the processor and reports every moment the
//! gain reduction *grows* shortly after the detector drops - which is the thing being complained
//! about. A synthetic steady-tone bed does not reproduce it, so this uses the real thing.

use com_is_assist_core::automix::{AutomixEngineConfig, AutomixProcessor};
use com_is_assist_core::voice_activity::{SileroVad, VoiceActivityConfig};

const BLOCK: usize = 64;

fn read_mono(path: &str) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => reader.samples::<i32>().map(|s| s.unwrap() as f32 * scale).collect(),
    };
    (samples, spec.sample_rate)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let com_path = args.next().expect("usage: release_overshoot <com.wav> <bed.wav> [seconds]");
    let bed_path = args.next().expect("usage: release_overshoot <com.wav> <bed.wav> [seconds]");
    let limit: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(600.0);

    let (com, rate) = read_mono(&com_path);
    let (bed, bed_rate) = read_mono(&bed_path);
    assert_eq!(rate, bed_rate, "files must share a sample rate");
    let n = com.len().min(bed.len()).min((limit * rate as f64) as usize);
    println!("{:.0}s of real material at {rate}Hz\n", n as f64 / rate as f64);

    let mut vad = SileroVad::new(rate, VoiceActivityConfig::default()).expect("vad");
    let mut processor =
        AutomixProcessor::new(1, rate, AutomixEngineConfig::default(), 0.1).expect("processor");

    // Measured from where the *speech energy* stops, not where the detector drops. Those are 800ms
    // apart - the hangover deliberately holds the ducking state through inter-sentence pauses - and
    // it is that window the complaint is about: the commentator has stopped, and the bed goes
    // *further* down before it recovers.
    let quiet_level = 10f32.powf(-55.0 / 20.0);
    let mut quiet_blocks = 0usize;
    let quiet_needed = (0.150 * rate as f64) as usize / BLOCK;
    let mut stopped_at: Option<usize> = None;
    let mut gr_at_stop = 0.0_f64;
    let mut worst = (0.0_f64, 0.0_f64, 0.0_f64);
    let mut events: Vec<(f64, f64, f64)> = Vec::new();
    let (mut falling_edges, mut max_gr) = (0u32, 0.0_f64);
    let mut was_active = false;
    let trace_mode = std::env::var("TRACE").is_ok();

    for start in (0..n - BLOCK).step_by(BLOCK) {
        let d = &com[start..start + BLOCK];
        vad.feed(d).expect("feed");
        let active = vad.voice_active();
        processor.feed_dialogue(d).expect("dialogue");
        processor.set_voice_activity(active, false);
        let mut b = bed[start..start + BLOCK].to_vec();
        processor.process_bed(&mut b).expect("bed");
        processor.maybe_run_control_step(active, false);
        let gr = processor.applied_gain_reduction_db();
        max_gr = max_gr.max(gr);
        if was_active && !active {
            falling_edges += 1;
        }

        let rms = (d.iter().map(|s| s * s).sum::<f32>() / BLOCK as f32).sqrt();
        if rms < quiet_level {
            quiet_blocks += 1;
            // The moment speech stopped, recorded once, while the detector is still holding.
            if quiet_blocks == quiet_needed && active && gr > 1.0 {
                stopped_at = Some(start);
                gr_at_stop = gr;
                worst = (0.0, 0.0, gr);
            }
        } else {
            quiet_blocks = 0;
            if stopped_at.take().is_some() && worst.0 > 0.05 {
                events.push(worst);
            }
        }
        if let Some(at) = stopped_at {
            let ms = (start - at) as f64 / rate as f64 * 1000.0;
            let extra = gr - gr_at_stop;
            if extra > worst.0 {
                worst = (extra, ms, gr);
            }
            if ms > 1500.0 {
                if worst.0 > 0.05 {
                    events.push(worst);
                }
                stopped_at = None;
            }
        }
        if trace_mode {
            let from = std::env::var("TRACE").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(61.0);
            let t = start as f64 / rate as f64;
            if t >= from && t <= from + 4.0 && (start / BLOCK).is_multiple_of(40) {
                    let c = processor.contributions();
                    let com_rms = 20.0 * (rms.max(1e-9) as f64).log10();
                    println!(
                        "  t={:7.2}s com={com_rms:6.1}dB vad={active:5} {:?}  slow {:6.2} mid {:6.2} fast {:6.2} -> gr {:6.2}",
                        t, processor.mix_state(), c.slow_db, c.mid_db, c.fast_db, gr
                    );
            }
        }
        was_active = active;
    }

    println!("detector falling edges: {falling_edges}, peak gain reduction seen: {max_gr:.2} dB");
    println!("speech stops observed with extra ducking: {}", events.len());
    if events.is_empty() {
        println!("no extra ducking after any phrase ending");
        return;
    }
    events.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    let mean = events.iter().map(|e| e.0).sum::<f64>() / events.len() as f64;
    println!("extra ducking after speech stops: mean {:+.2} dB, worst {:+.2} dB", mean, events[0].0);
    println!("\nworst offenders (extra dB, when, reduction reached):");
    for (extra, ms, gr) in events.iter().take(8) {
        println!("  {extra:+.2} dB at +{ms:.0}ms  (reached {gr:.2} dB)");
    }
}

//! Reads a WAV and prints momentary/short-term from the incremental meter and from `ebur128`
//! side by side, so a disagreement on real material shows up rather than only on test tones.
use com_is_assist_core::automix::KWeightedLoudnessMeter;
use com_is_assist_core::loudness::Ebur128Meter;

fn main() {
    let path = std::env::args().nth(1).expect("usage: compare_meters <file.wav>");
    let mut reader = hound::WavReader::open(&path).expect("open wav");
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.unwrap() as f32 * scale).collect()
        }
    };
    println!("{path}: {channels}ch {}Hz, {} frames", spec.sample_rate, samples.len() / channels);

    let map = Ebur128Meter::bed_channel_map(channels as u32).expect("channel map");
    let mut ours = KWeightedLoudnessMeter::new(&map, spec.sample_rate);
    let mut r128 = Ebur128Meter::new(&map, spec.sample_rate).unwrap();

    let tick = spec.sample_rate as usize / 10 * channels;
    let (mut worst_m, mut worst_s) = (0.0_f64, 0.0_f64);
    for (i, chunk) in samples.chunks(tick).enumerate() {
        ours.push_frames(chunk);
        r128.push_frames(chunk).unwrap();
        let (om, rm) = (ours.momentary_loudness_db(), r128.momentary_loudness_db());
        let (os, rs) = (ours.short_term_loudness_db(), r128.short_term_loudness_db());
        if i >= 30 {
            worst_m = worst_m.max((om - rm).abs());
            worst_s = worst_s.max((os - rs).abs());
        }
        if i % 20 == 0 {
            println!("t={:5.1}s  momentary ours {om:8.2} r128 {rm:8.2}   short-term ours {os:8.2} r128 {rs:8.2}", i as f64 / 10.0);
        }
    }
    println!("\nworst disagreement after warm-up: momentary {worst_m:.3} dB, short-term {worst_s:.3} dB");
}

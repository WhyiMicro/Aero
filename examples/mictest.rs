//! Diagnostic: listens to every input device for a few seconds and prints the level of each
//! channel, so you can see which device/input your voice actually arrives on.
//! Run with: cargo run --example mictest -- [seconds]

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Stats {
    sum_sq: Vec<f64>,
    peak: Vec<f32>,
    frames: u64,
}

fn db(x: f64) -> f64 {
    if x <= 0.0 { -200.0 } else { 20.0 * x.log10() }
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(15);
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.description().ok()).map(|d| d.name().to_string());
    let mut running = Vec::new();

    for dev in host.input_devices().expect("listing input devices") {
        let name = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
        let Ok(sup) = dev.default_input_config() else {
            println!("{name}: no usable config");
            continue;
        };
        let ch = sup.channels() as usize;
        let stats = Arc::new(Mutex::new(Stats { sum_sq: vec![0.0; ch], peak: vec![0.0; ch], frames: 0 }));
        let s = stats.clone();
        let on_data = move |data: &[f32]| {
            let mut st = s.lock().unwrap();
            for frame in data.chunks_exact(ch) {
                for (c, v) in frame.iter().enumerate() {
                    st.sum_sq[c] += (*v as f64) * (*v as f64);
                    st.peak[c] = st.peak[c].max(v.abs());
                }
                st.frames += 1;
            }
        };
        let err = |e| eprintln!("stream error: {e}");
        let stream = match sup.sample_format() {
            cpal::SampleFormat::F32 => dev.build_input_stream::<f32, _, _>(sup.config(), move |d, _| on_data(d), err, None),
            cpal::SampleFormat::I16 => dev.build_input_stream::<i16, _, _>(
                sup.config(),
                move |d: &[i16], _| on_data(&d.iter().map(|s| *s as f32 / 32768.0).collect::<Vec<_>>()),
                err,
                None,
            ),
            f => {
                println!("{name}: unsupported format {f:?}");
                continue;
            }
        };
        match stream {
            Ok(st) => {
                if st.play().is_ok() {
                    running.push((name, sup.sample_rate(), ch, stats, st));
                }
            }
            Err(e) => println!("{name}: can't open ({e})"),
        }
    }

    println!("Listening to {} input devices for {secs}s...", running.len());
    std::thread::sleep(Duration::from_secs(secs));

    for (name, rate, ch, stats, _stream) in &running {
        let st = stats.lock().unwrap();
        let tag = if default_name.as_deref() == Some(name.as_str()) { "  [Windows default]" } else { "" };
        println!("\n{name}  ({rate} Hz, {ch} ch, {} frames){tag}", st.frames);
        for c in 0..*ch {
            let rms = if st.frames > 0 { (st.sum_sq[c] / st.frames as f64).sqrt() } else { 0.0 };
            println!("  input {}: RMS {:>7.1} dB   peak {:>7.1} dB", c + 1, db(rms), db(st.peak[c] as f64));
        }
    }
}

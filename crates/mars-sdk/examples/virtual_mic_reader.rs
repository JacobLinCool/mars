//! Acceptance-test reader: captures from the MARS virtual input through the
//! real CoreAudio stack and verifies the 440 Hz acceptance tone via a
//! Goertzel filter (issue #41).
//!
//! Usage: `cargo run -p mars-sdk --example virtual_mic_reader -- <device-uid> [expect-silence]`
//!
//! Exit codes: 0 = expectation met, 1 = expectation failed, 2 = device not
//! found (driver not installed / not applied).

use std::f64::consts::TAU;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const TONE_HZ: f64 = 440.0;
const CAPTURE_SECONDS: f64 = 2.0;

fn goertzel_power(samples: &[f32], sample_rate: f64, target_hz: f64) -> f64 {
    let coefficient = 2.0 * (TAU * target_hz / sample_rate).cos();
    let (mut s_prev, mut s_prev2) = (0.0_f64, 0.0_f64);
    for sample in samples {
        let s = f64::from(*sample) + coefficient * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    (s_prev2 * s_prev2 + s_prev * s_prev - coefficient * s_prev * s_prev2)
        / (samples.len().max(1) as f64)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(target_uid) = args.next() else {
        eprintln!("usage: virtual_mic_reader <device-uid> [expect-silence]");
        std::process::exit(2);
    };
    let expect_silence = args.next().as_deref() == Some("expect-silence");

    let host = cpal::default_host();
    let Some(device) = host.input_devices().ok().and_then(|mut devices| {
        devices.find(|device| {
            device
                .name()
                .map(|name| name.contains("MARS") || name.contains("Acceptance"))
                .unwrap_or(false)
        })
    }) else {
        eprintln!(
            "virtual input device not found (uid {target_uid}); is mars.driver installed and applied?"
        );
        std::process::exit(2);
    };

    let config = match device.default_input_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("cannot read input config: {error}");
            std::process::exit(2);
        }
    };
    let sample_rate = f64::from(config.sample_rate());
    let captured: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);

    let stream = match device.build_input_stream(
        &config.into(),
        move |data: &[f32], _| {
            if let Ok(mut sink) = sink.lock() {
                sink.extend_from_slice(data);
            }
        },
        |error| eprintln!("stream error: {error}"),
        None,
    ) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("cannot open input stream: {error}");
            std::process::exit(2);
        }
    };

    if let Err(error) = stream.play() {
        eprintln!("cannot start input stream: {error}");
        std::process::exit(2);
    }
    std::thread::sleep(Duration::from_secs_f64(CAPTURE_SECONDS));
    drop(stream);

    let samples = captured.lock().map(|sink| sink.clone()).unwrap_or_default();
    if samples.len() < (sample_rate * 0.5) as usize {
        eprintln!("captured too few samples: {}", samples.len());
        std::process::exit(1);
    }

    let tone = goertzel_power(&samples, sample_rate, TONE_HZ);
    let off_tone = goertzel_power(&samples, sample_rate, TONE_HZ * 1.71);
    let rms = (samples
        .iter()
        .map(|s| f64::from(*s) * f64::from(*s))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt();
    println!(
        "captured {} samples, rms={rms:.6}, 440Hz power={tone:.6}, off-tone power={off_tone:.6}",
        samples.len()
    );

    let pass = if expect_silence {
        rms < 1.0e-4
    } else {
        rms > 1.0e-3 && tone > off_tone * 10.0
    };
    if pass {
        println!("PASS");
    } else {
        eprintln!(
            "FAIL: expectation {} not met",
            if expect_silence {
                "silence"
            } else {
                "440Hz tone"
            }
        );
        std::process::exit(1);
    }
}

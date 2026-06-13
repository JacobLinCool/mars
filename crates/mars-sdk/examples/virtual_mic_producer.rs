//! Acceptance-test producer: ensures an app-owned virtual input and streams
//! a 440 Hz sine through the live writer until killed (issue #41).
//!
//! Usage: `cargo run -p mars-sdk --example virtual_mic_producer [seconds]`
//!
//! Pacing follows wall-clock absolute deadlines so the ring always holds
//! fresh audio for the HAL consumer.

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use mars_sdk::{AppVirtualInput, MarsClient, ProducerKind};

const SAMPLE_RATE: u32 = 48_000;
const TONE_HZ: f32 = 440.0;
const CHUNK_FRAMES: usize = 480; // 10 ms

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(10);

    let client = MarsClient::new_default(MarsClient::default_timeout())?;
    let mic = client
        .ensure_virtual_input(AppVirtualInput {
            app_id: "com.mars.acceptance".into(),
            id: "acceptance-mic".into(),
            name: "MARS Acceptance Mic".into(),
            uid: "com.mars.acceptance.mic".into(),
            sample_rate: SAMPLE_RATE,
            channels: 1,
            producer: ProducerKind::ExternalApp,
        })
        .await?;
    println!(
        "ensured device uid={} ring={}",
        mic.uid(),
        mic.info().ring_name
    );

    let mut writer = mic.open_live_writer()?;
    let mut phase = 0.0_f32;
    let chunk_period = Duration::from_millis(10);
    let mut next_deadline = Instant::now();
    let total_chunks = seconds as usize * 100;

    let mut chunk = vec![0.0_f32; CHUNK_FRAMES];
    for index in 0..total_chunks {
        for sample in chunk.iter_mut() {
            *sample = 0.5 * phase.sin();
            phase = (phase + TAU * TONE_HZ / SAMPLE_RATE as f32) % TAU;
        }
        writer.write_f32_interleaved_live(&chunk)?;
        if index % 100 == 0 {
            println!("streamed {}s", index / 100);
        }
        next_deadline += chunk_period;
        let now = Instant::now();
        if next_deadline > now {
            std::thread::sleep(next_deadline - now);
        } else {
            next_deadline = now + chunk_period;
        }
    }

    writer.flush_silence()?;
    println!("done; flushed silence and detaching");
    Ok(())
}

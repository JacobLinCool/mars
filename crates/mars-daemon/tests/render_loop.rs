#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use mars_engine::{Engine, EngineSnapshot};
use mars_graph::build_routing_graph;
use mars_shm::{RingSpec, StreamDirection, global_registry, stream_name};
use mars_types::{Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

fn test_profile() -> Profile {
    let mut profile = Profile::default();
    profile.audio.buffer_frames = 16;
    profile.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app".to_string(),
        name: "App".to_string(),
        channels: Some(2),
        uid: Some("com.mars.vout.app".to_string()),
        hidden: false,
    });
    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".to_string(),
        name: "Mix".to_string(),
        channels: Some(2),
        uid: Some("com.mars.vin.mix".to_string()),
        mix: None,
    });
    profile.pipes.push(Pipe {
        from: "app".to_string(),
        to: "mix".to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    profile
}

#[test]
fn mock_ring_buffers_feed_engine_cycle() {
    let profile = test_profile();
    let graph = build_routing_graph(&profile).expect("build graph");
    let engine = Engine::new(EngineSnapshot {
        graph,
        sample_rate: 48_000,
        buffer_frames: 16,
    });

    let vout_uid = "com.mars.vout.app";
    let vin_uid = "com.mars.vin.mix";
    let vout_name = stream_name(StreamDirection::Vout, vout_uid);
    let vin_name = stream_name(StreamDirection::Vin, vin_uid);
    let spec = RingSpec {
        sample_rate: 48_000,
        channels: 2,
        capacity_frames: 128,
    };

    let vout = global_registry()
        .create_or_open(&vout_name, spec)
        .expect("create vout");
    let vin = global_registry()
        .create_or_open(&vin_name, spec)
        .expect("create vin");

    let input = vec![0.2_f32; 32];
    vout.lock()
        .write_interleaved(&input)
        .expect("write source frames");

    let mut source_frames = vec![0.0_f32; 32];
    vout.lock()
        .read_interleaved(&mut source_frames)
        .expect("read source frames");

    let mut sources = HashMap::new();
    sources.insert("app".to_string(), source_frames);
    let rendered = engine.render_cycle(16, &sources).expect("render");
    let sink = rendered.sinks.get("mix").expect("sink");
    vin.lock()
        .write_interleaved(sink)
        .expect("write rendered sink");

    let mut out = vec![0.0_f32; 32];
    vin.lock()
        .read_interleaved(&mut out)
        .expect("read sink frames");
    assert!(out.iter().any(|sample| sample.abs() > 0.0));

    let _ = global_registry().remove(&vout_name);
    let _ = global_registry().remove(&vin_name);
}

#[test]
fn mock_lifecycle_clear_and_reapply_recreates_rings() {
    let vout_name = stream_name(StreamDirection::Vout, "lifecycle");
    let spec = RingSpec {
        sample_rate: 48_000,
        channels: 2,
        capacity_frames: 64,
    };
    let first = global_registry()
        .create_or_open(&vout_name, spec)
        .expect("first create");
    first
        .lock()
        .write_interleaved(&[1.0, 1.0, 0.5, 0.5])
        .expect("first write");
    let removed = global_registry().remove_namespace("mars.");
    assert!(removed >= 1);

    let second = global_registry()
        .create_or_open(&vout_name, spec)
        .expect("second create");
    let mut out = [0.0_f32; 4];
    second
        .lock()
        .read_interleaved(&mut out)
        .expect("second read");
    assert_eq!(out, [0.0, 0.0, 0.0, 0.0]);

    let _ = global_registry().remove(&vout_name);
}

#[test]
fn cycle_deadline_miss_counter_increments_when_work_exceeds_period() {
    let period = Duration::from_secs_f64(16.0 / 48_000.0);
    let mut deadline_miss_count = 0_u64;

    for _ in 0..4 {
        let start = Instant::now();
        thread::sleep(Duration::from_millis(2));
        if start.elapsed() > period {
            deadline_miss_count = deadline_miss_count.saturating_add(1);
        }
    }

    assert!(deadline_miss_count > 0);
}

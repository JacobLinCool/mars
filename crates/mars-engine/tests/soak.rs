#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use mars_engine::{Engine, EngineSnapshot, RenderOutput};
use mars_graph::build_routing_graph;
use mars_types::{Bus, MixConfig, MixMode, Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

fn profile_variant(master_to_stream_delay_ms: f32, stream_limiter: bool) -> Profile {
    let mut profile = Profile::default();
    for id in ["app1", "app2", "app3", "app4", "app5", "app6"] {
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: id.to_string(),
            name: id.to_string(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
    }

    profile.buses.push(Bus {
        id: "music".to_string(),
        channels: Some(2),
        mix: None,
    });
    profile.buses.push(Bus {
        id: "voice".to_string(),
        channels: Some(2),
        mix: None,
    });
    profile.buses.push(Bus {
        id: "master".to_string(),
        channels: Some(2),
        mix: None,
    });

    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "stream".to_string(),
        name: "Stream".to_string(),
        channels: Some(2),
        uid: None,
        mix: Some(MixConfig {
            limiter: stream_limiter,
            limit_dbfs: -1.0,
            mode: MixMode::Sum,
        }),
    });
    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "monitor".to_string(),
        name: "Monitor".to_string(),
        channels: Some(2),
        uid: None,
        mix: Some(MixConfig {
            limiter: false,
            limit_dbfs: -1.0,
            mode: MixMode::Average,
        }),
    });
    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "record".to_string(),
        name: "Record".to_string(),
        channels: Some(2),
        uid: None,
        mix: Some(MixConfig {
            limiter: false,
            limit_dbfs: -1.0,
            mode: MixMode::Sum,
        }),
    });

    for source in ["app1", "app2", "app3"] {
        profile.pipes.push(Pipe {
            from: source.to_string(),
            to: "music".to_string(),
            enabled: true,
            gain_db: -3.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
    }
    for source in ["app4", "app5", "app6"] {
        profile.pipes.push(Pipe {
            from: source.to_string(),
            to: "voice".to_string(),
            enabled: true,
            gain_db: -6.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
    }

    profile.pipes.push(Pipe {
        from: "music".to_string(),
        to: "master".to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: -0.15,
        delay_ms: 0.0,
    });
    profile.pipes.push(Pipe {
        from: "voice".to_string(),
        to: "master".to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.15,
        delay_ms: 0.0,
    });
    profile.pipes.push(Pipe {
        from: "master".to_string(),
        to: "stream".to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: master_to_stream_delay_ms,
    });
    profile.pipes.push(Pipe {
        from: "master".to_string(),
        to: "monitor".to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    profile.pipes.push(Pipe {
        from: "master".to_string(),
        to: "record".to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });

    profile
}

fn snapshot_from_profile(
    profile: &Profile,
    sample_rate: u32,
    buffer_frames: u32,
) -> EngineSnapshot {
    let graph = build_routing_graph(profile).expect("routing graph");
    EngineSnapshot {
        graph,
        sample_rate,
        buffer_frames,
    }
}

fn stereo_source(frames: usize, phase: f32) -> Vec<f32> {
    (0..frames * 2)
        .map(|i| ((i as f32 * 0.003) + phase).sin() * 0.5)
        .collect()
}

fn make_sources(cycle: usize, frames: usize) -> HashMap<String, Vec<f32>> {
    let mut sources = HashMap::new();
    for (idx, id) in ["app1", "app2", "app3", "app4", "app5", "app6"]
        .iter()
        .enumerate()
    {
        let phase = cycle as f32 * 0.01 + idx as f32;
        sources.insert((*id).to_string(), stereo_source(frames, phase));
    }
    sources
}

fn assert_valid_sinks(output: &RenderOutput, frames: usize) {
    for sink_id in ["stream", "monitor", "record"] {
        let sink = output.sinks.get(sink_id).expect("sink must exist");
        assert_eq!(sink.len(), frames * 2);
        assert!(sink.iter().all(|sample| sample.is_finite()));
    }
}

#[test]
fn soak_cycles_with_snapshot_swaps_keep_outputs_valid() {
    let snapshot_a = snapshot_from_profile(&profile_variant(0.0, true), 48_000, 256);
    let snapshot_b = snapshot_from_profile(&profile_variant(8.0, false), 48_000, 256);
    let engine = Engine::new(snapshot_a.clone());
    let frames = 256;

    for cycle in 0..1_500 {
        if cycle > 0 && cycle % 150 == 0 {
            let next = if (cycle / 150) % 2 == 0 {
                snapshot_a.clone()
            } else {
                snapshot_b.clone()
            };
            engine.swap_snapshot(next);
        }

        let sources = make_sources(cycle, frames);
        let output = engine.render_cycle(frames, &sources).expect("render cycle");
        assert_valid_sinks(&output, frames);
    }
}

#[test]
fn concurrent_render_and_snapshot_swap_stays_stable() {
    let snapshot_a = snapshot_from_profile(&profile_variant(0.0, true), 48_000, 128);
    let snapshot_b = snapshot_from_profile(&profile_variant(6.0, false), 48_000, 128);
    let engine = Arc::new(Engine::new(snapshot_a.clone()));

    let render_engine = Arc::clone(&engine);
    let render_thread = thread::spawn(move || {
        let frames = 128;
        for cycle in 0..2_000 {
            let sources = make_sources(cycle, frames);
            let output = render_engine
                .render_cycle(frames, &sources)
                .expect("render cycle");
            assert_valid_sinks(&output, frames);
        }
    });

    let swap_engine = Arc::clone(&engine);
    let swap_thread = thread::spawn(move || {
        for step in 0..600 {
            let snapshot = if step % 2 == 0 {
                snapshot_b.clone()
            } else {
                snapshot_a.clone()
            };
            swap_engine.swap_snapshot(snapshot);
        }
    });

    render_thread.join().expect("render thread");
    swap_thread.join().expect("snapshot thread");
}

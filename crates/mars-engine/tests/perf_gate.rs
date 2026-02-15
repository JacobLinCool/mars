#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::time::Instant;

use mars_engine::{Engine, EngineSnapshot};
use mars_graph::build_routing_graph;
use mars_types::{Bus, MixConfig, MixMode, Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

fn perf_profile() -> Profile {
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
            limiter: true,
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
        delay_ms: 0.0,
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

fn make_sources(frames: usize) -> HashMap<String, Vec<f32>> {
    let mut sources = HashMap::new();
    for (idx, id) in ["app1", "app2", "app3", "app4", "app5", "app6"]
        .iter()
        .enumerate()
    {
        let phase = idx as f32 * 0.4;
        let data: Vec<f32> = (0..frames * 2)
            .map(|i| ((i as f32 * 0.003) + phase).sin() * 0.5)
            .collect();
        sources.insert((*id).to_string(), data);
    }
    sources
}

#[test]
#[ignore = "release-only perf regression gate; run with --release -- --ignored"]
fn release_render_multisource_multioutput_stays_under_budget() {
    if cfg!(debug_assertions) {
        return;
    }

    let graph = build_routing_graph(&perf_profile()).expect("routing graph");
    let engine = Engine::new(EngineSnapshot {
        graph,
        sample_rate: 48_000,
        buffer_frames: 256,
    });
    let sources = make_sources(256);

    for _ in 0..3_000 {
        engine
            .render_cycle(256, &sources)
            .expect("warmup render cycle");
    }

    let mut batch_averages_us = Vec::with_capacity(5);
    for _ in 0..5 {
        let started = Instant::now();
        for _ in 0..5_000 {
            engine
                .render_cycle(256, &sources)
                .expect("timed render cycle");
        }
        let avg_us = started.elapsed().as_secs_f64() * 1_000_000.0 / 5_000.0;
        batch_averages_us.push(avg_us);
    }

    batch_averages_us.sort_by(|a, b| a.total_cmp(b));
    let median_us = batch_averages_us[batch_averages_us.len() / 2];
    let worst_us = batch_averages_us.iter().copied().fold(0.0_f64, f64::max);

    // Conservative thresholds to avoid CI hardware noise while still catching large regressions.
    assert!(
        median_us <= 250.0,
        "median render time {median_us:.2}us exceeds 250us budget"
    );
    assert!(
        worst_us <= 350.0,
        "worst batch average {worst_us:.2}us exceeds 350us budget"
    );
}

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mars_engine::{Engine, EngineSnapshot};
use mars_graph::build_routing_graph;
use mars_types::{Bus, MixConfig, MixMode, Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

fn simple_profile() -> Profile {
    let mut p = Profile::default();
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app".into(),
        name: "App".into(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".into(),
        name: "Mix".into(),
        channels: Some(2),
        uid: None,
        mix: Some(MixConfig {
            limiter: false,
            limit_dbfs: -1.0,
            mode: MixMode::Sum,
        }),
    });
    p.pipes.push(Pipe {
        from: "app".into(),
        to: "mix".into(),
        enabled: true,
        gain_db: -6.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p
}

fn complex_profile() -> Profile {
    let mut p = Profile::default();
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app1".into(),
        name: "App 1".into(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app2".into(),
        name: "App 2".into(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    p.buses.push(Bus {
        id: "bus".into(),
        channels: Some(2),
        mix: Some(MixConfig {
            limiter: false,
            limit_dbfs: -1.0,
            mode: MixMode::Sum,
        }),
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "sink".into(),
        name: "Sink".into(),
        channels: Some(2),
        uid: None,
        mix: Some(MixConfig {
            limiter: true,
            limit_dbfs: -1.0,
            mode: MixMode::Sum,
        }),
    });
    p.pipes.push(Pipe {
        from: "app1".into(),
        to: "bus".into(),
        enabled: true,
        gain_db: -6.0,
        mute: false,
        pan: -0.5,
        delay_ms: 0.0,
    });
    p.pipes.push(Pipe {
        from: "app2".into(),
        to: "bus".into(),
        enabled: true,
        gain_db: -3.0,
        mute: false,
        pan: 0.5,
        delay_ms: 0.0,
    });
    p.pipes.push(Pipe {
        from: "bus".into(),
        to: "sink".into(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p
}

fn delay_profile() -> Profile {
    let mut p = Profile::default();
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app".into(),
        name: "App".into(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".into(),
        name: "Mix".into(),
        channels: Some(2),
        uid: None,
        mix: None,
    });
    p.pipes.push(Pipe {
        from: "app".into(),
        to: "mix".into(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 10.0,
    });
    p
}

fn channel_conversion_profile() -> Profile {
    let mut p = Profile::default();
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "mono-src".into(),
        name: "Mono Source".into(),
        channels: Some(1),
        uid: None,
        hidden: false,
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "stereo-sink".into(),
        name: "Stereo Sink".into(),
        channels: Some(2),
        uid: None,
        mix: None,
    });
    p.pipes.push(Pipe {
        from: "mono-src".into(),
        to: "stereo-sink".into(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p
}

fn make_engine(profile: &Profile) -> Engine {
    let graph = build_routing_graph(profile).unwrap();
    Engine::new(EngineSnapshot {
        graph,
        sample_rate: 48_000,
        buffer_frames: 256,
    })
}

fn stereo_source(id: &str, frames: usize) -> (String, Vec<f32>) {
    let samples: Vec<f32> = (0..frames * 2).map(|i| (i as f32 * 0.001).sin()).collect();
    (id.to_string(), samples)
}

fn mono_source(id: &str, frames: usize) -> (String, Vec<f32>) {
    let samples: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.001).sin()).collect();
    (id.to_string(), samples)
}

fn bench_render_simple(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_simple");
    let profile = simple_profile();
    let engine = make_engine(&profile);

    for frames in [64, 256, 1024] {
        let sources: HashMap<String, Vec<f32>> =
            [stereo_source("app", frames)].into_iter().collect();
        group.bench_with_input(
            BenchmarkId::from_parameter(frames),
            &(frames, &sources),
            |b, (f, s)| {
                b.iter(|| engine.render_cycle(*f, s).unwrap());
            },
        );
    }

    group.finish();
}

fn bench_render_complex(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_complex");
    let profile = complex_profile();
    let engine = make_engine(&profile);

    for frames in [64, 256, 1024] {
        let sources: HashMap<String, Vec<f32>> =
            [stereo_source("app1", frames), stereo_source("app2", frames)]
                .into_iter()
                .collect();
        group.bench_with_input(
            BenchmarkId::from_parameter(frames),
            &(frames, &sources),
            |b, (f, s)| {
                b.iter(|| engine.render_cycle(*f, s).unwrap());
            },
        );
    }

    group.finish();
}

fn bench_render_with_delay(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_with_delay");
    let profile = delay_profile();
    let engine = make_engine(&profile);

    for frames in [64, 256, 1024] {
        let sources: HashMap<String, Vec<f32>> =
            [stereo_source("app", frames)].into_iter().collect();
        group.bench_with_input(
            BenchmarkId::from_parameter(frames),
            &(frames, &sources),
            |b, (f, s)| {
                b.iter(|| engine.render_cycle(*f, s).unwrap());
            },
        );
    }

    group.finish();
}

fn bench_render_channel_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_channel_conversion");
    let profile = channel_conversion_profile();
    let engine = make_engine(&profile);

    for frames in [64, 256, 1024] {
        let sources: HashMap<String, Vec<f32>> =
            [mono_source("mono-src", frames)].into_iter().collect();
        group.bench_with_input(
            BenchmarkId::from_parameter(frames),
            &(frames, &sources),
            |b, (f, s)| {
                b.iter(|| engine.render_cycle(*f, s).unwrap());
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_render_simple,
    bench_render_complex,
    bench_render_with_delay,
    bench_render_channel_conversion
);
criterion_main!(benches);

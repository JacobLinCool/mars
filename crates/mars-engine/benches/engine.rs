#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_engine::{Engine, EngineSnapshot, ProcessorControl};
use mars_graph::build_routing_graph;
use mars_types::{
    Bus, MixConfig, MixMode, Pipe, ProcessorChain, ProcessorDefinition, ProcessorKind, Profile,
    Route, RouteMatrix, VirtualInputDevice, VirtualOutputDevice,
};
use serde_json::json;

fn identity_matrix(channels: u16) -> RouteMatrix {
    let channels = channels as usize;
    let mut coefficients = vec![vec![0.0; channels]; channels];
    for (index, row) in coefficients.iter_mut().enumerate() {
        row[index] = 1.0;
    }
    RouteMatrix {
        rows: channels as u16,
        cols: channels as u16,
        coefficients,
    }
}

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

fn multisource_multioutput_profile() -> Profile {
    let mut p = Profile::default();
    for id in ["app1", "app2", "app3", "app4", "app5", "app6"] {
        p.virtual_devices.outputs.push(VirtualOutputDevice {
            id: id.into(),
            name: id.into(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
    }

    p.buses.push(Bus {
        id: "music".into(),
        channels: Some(2),
        mix: None,
    });
    p.buses.push(Bus {
        id: "voice".into(),
        channels: Some(2),
        mix: None,
    });
    p.buses.push(Bus {
        id: "master".into(),
        channels: Some(2),
        mix: None,
    });

    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "stream".into(),
        name: "Stream".into(),
        channels: Some(2),
        uid: None,
        mix: Some(MixConfig {
            limiter: true,
            limit_dbfs: -1.0,
            mode: MixMode::Sum,
        }),
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "monitor".into(),
        name: "Monitor".into(),
        channels: Some(2),
        uid: None,
        mix: Some(MixConfig {
            limiter: false,
            limit_dbfs: -1.0,
            mode: MixMode::Average,
        }),
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "record".into(),
        name: "Record".into(),
        channels: Some(2),
        uid: None,
        mix: Some(MixConfig {
            limiter: false,
            limit_dbfs: -1.0,
            mode: MixMode::Sum,
        }),
    });

    for source in ["app1", "app2", "app3"] {
        p.pipes.push(Pipe {
            from: source.into(),
            to: "music".into(),
            enabled: true,
            gain_db: -3.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
    }
    for source in ["app4", "app5", "app6"] {
        p.pipes.push(Pipe {
            from: source.into(),
            to: "voice".into(),
            enabled: true,
            gain_db: -6.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
    }
    p.pipes.push(Pipe {
        from: "music".into(),
        to: "master".into(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: -0.15,
        delay_ms: 0.0,
    });
    p.pipes.push(Pipe {
        from: "voice".into(),
        to: "master".into(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.15,
        delay_ms: 0.0,
    });
    p.pipes.push(Pipe {
        from: "master".into(),
        to: "stream".into(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p.pipes.push(Pipe {
        from: "master".into(),
        to: "monitor".into(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p.pipes.push(Pipe {
        from: "master".into(),
        to: "record".into(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });

    p
}

fn matrix_profile(channels: u16) -> Profile {
    let mut p = Profile::default();
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "matrix-src".into(),
        name: "Matrix Source".into(),
        channels: Some(channels),
        uid: None,
        hidden: false,
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "matrix-sink".into(),
        name: "Matrix Sink".into(),
        channels: Some(channels),
        uid: None,
        mix: None,
    });
    p.routes.push(Route {
        id: "matrix-route".into(),
        from: "matrix-src".into(),
        to: "matrix-sink".into(),
        enabled: true,
        matrix: identity_matrix(channels),
        chain: None,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p
}

fn processor_config(kind: ProcessorKind) -> serde_json::Value {
    match kind {
        ProcessorKind::Eq => json!({
            "bands": [
                {
                    "freq_hz": 1200.0,
                    "q": 1.0,
                    "gain_db": 3.0,
                    "enabled": true
                }
            ]
        }),
        ProcessorKind::Dynamics => json!({
            "threshold_db": -18.0,
            "ratio": 4.0,
            "attack_ms": 10.0,
            "release_ms": 100.0,
            "makeup_gain_db": 0.0,
            "limiter": false
        }),
        _ => serde_json::Value::Null,
    }
}

fn chain_profile(kinds: &[ProcessorKind]) -> Profile {
    let mut p = Profile::default();
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "chain-src".into(),
        name: "Chain Source".into(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "chain-sink".into(),
        name: "Chain Sink".into(),
        channels: Some(2),
        uid: None,
        mix: None,
    });

    let mut processor_ids = Vec::with_capacity(kinds.len());
    for (index, kind) in kinds.iter().copied().enumerate() {
        let processor_id = format!("proc-{index}");
        p.processors.push(ProcessorDefinition {
            id: processor_id.clone(),
            kind,
            config: processor_config(kind),
        });
        processor_ids.push(processor_id);
    }
    p.processor_chains.push(ProcessorChain {
        id: "main-chain".into(),
        processors: processor_ids,
    });
    p.routes.push(Route {
        id: "chain-route".into(),
        from: "chain-src".into(),
        to: "chain-sink".into(),
        enabled: true,
        matrix: identity_matrix(2),
        chain: Some("main-chain".into()),
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

fn multichannel_source(id: &str, frames: usize, channels: usize) -> (String, Vec<f32>) {
    let samples: Vec<f32> = (0..frames * channels)
        .map(|i| (i as f32 * 0.0007).sin())
        .collect();
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

fn bench_render_multisource_multioutput(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_multisource_multioutput");
    let profile = multisource_multioutput_profile();
    let engine = make_engine(&profile);

    for frames in [128, 256, 512] {
        let sources: HashMap<String, Vec<f32>> = [
            stereo_source("app1", frames),
            stereo_source("app2", frames),
            stereo_source("app3", frames),
            stereo_source("app4", frames),
            stereo_source("app5", frames),
            stereo_source("app6", frames),
        ]
        .into_iter()
        .collect();
        group.throughput(Throughput::Elements((frames * 2 * sources.len()) as u64));
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

fn bench_render_matrix(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_matrix");

    for channels in [8_u16, 16_u16, 32_u16] {
        let profile = matrix_profile(channels);
        let engine = make_engine(&profile);
        let frames = 256usize;
        let sources: HashMap<String, Vec<f32>> =
            [multichannel_source("matrix-src", frames, channels as usize)]
                .into_iter()
                .collect();

        group.throughput(Throughput::Elements((frames * channels as usize) as u64));
        group.bench_with_input(
            BenchmarkId::new(format!("{channels}x{channels}"), frames),
            &(frames, &sources),
            |b, (f, s)| {
                b.iter(|| engine.render_cycle(*f, s).unwrap());
            },
        );
    }

    group.finish();
}

fn bench_render_chain_length(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_chain_length");
    let frames = 256usize;

    for chain_length in [1usize, 4, 8, 16] {
        let profile = chain_profile(&vec![ProcessorKind::Eq; chain_length]);
        let engine = make_engine(&profile);
        let sources: HashMap<String, Vec<f32>> =
            [stereo_source("chain-src", frames)].into_iter().collect();

        group.bench_with_input(
            BenchmarkId::new(format!("{chain_length}"), frames),
            &(frames, &sources),
            |b, (f, s)| {
                b.iter(|| engine.render_cycle(*f, s).unwrap());
            },
        );
    }

    group.finish();
}

fn bench_render_param_updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_param_updates");
    let frames = 256usize;
    let profile = chain_profile(&[ProcessorKind::Eq; 8]);
    let engine = make_engine(&profile);
    let sources: HashMap<String, Vec<f32>> =
        [stereo_source("chain-src", frames)].into_iter().collect();
    let processor_ids = (0..8)
        .map(|index| format!("proc-{index}"))
        .collect::<Vec<_>>();

    for updates_per_cycle in [1usize, 8, 32] {
        group.bench_with_input(
            BenchmarkId::new(format!("{updates_per_cycle}"), frames),
            &(updates_per_cycle, &sources),
            |b, (updates, s)| {
                b.iter(|| {
                    for index in 0..*updates {
                        let processor_id = &processor_ids[index % processor_ids.len()];
                        engine.update_processor_control(
                            processor_id.as_str(),
                            ProcessorControl {
                                bypass: index % 2 == 0,
                                generation: index as u64,
                                params: BTreeMap::from([("amount".to_string(), index as f32)]),
                            },
                        );
                    }
                    engine.render_cycle(frames, s).unwrap();
                });
            },
        );
    }

    group.finish();
}

fn bench_render_processor_block(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_processor_block");
    let frames = 256usize;

    for (name, kind) in [
        ("eq", ProcessorKind::Eq),
        ("dynamics", ProcessorKind::Dynamics),
    ] {
        let profile = chain_profile(&[kind]);
        let engine = make_engine(&profile);
        let sources: HashMap<String, Vec<f32>> =
            [stereo_source("chain-src", frames)].into_iter().collect();
        group.bench_with_input(
            BenchmarkId::new(name, frames),
            &(frames, &sources),
            |b, (f, s)| {
                b.iter(|| engine.render_cycle(*f, s).unwrap());
            },
        );
    }

    group.finish();
}

fn bench_render_processor_chain_kind(c: &mut Criterion) {
    let mut group = c.benchmark_group("engine/render_processor_chain_kind");
    let frames = 256usize;
    let cases = vec![
        ("eqx4", vec![ProcessorKind::Eq; 4]),
        ("dynamicsx4", vec![ProcessorKind::Dynamics; 4]),
        (
            "mix8",
            vec![
                ProcessorKind::Eq,
                ProcessorKind::Dynamics,
                ProcessorKind::Eq,
                ProcessorKind::Dynamics,
                ProcessorKind::Eq,
                ProcessorKind::Dynamics,
                ProcessorKind::Eq,
                ProcessorKind::Dynamics,
            ],
        ),
    ];

    for (name, kinds) in cases {
        let profile = chain_profile(&kinds);
        let engine = make_engine(&profile);
        let sources: HashMap<String, Vec<f32>> =
            [stereo_source("chain-src", frames)].into_iter().collect();
        group.bench_with_input(
            BenchmarkId::new(name, frames),
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
    bench_render_channel_conversion,
    bench_render_multisource_multioutput,
    bench_render_matrix,
    bench_render_chain_length,
    bench_render_param_updates,
    bench_render_processor_block,
    bench_render_processor_chain_kind
);
criterion_main!(benches);

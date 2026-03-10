use std::collections::{HashMap, VecDeque};
use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_engine::{Engine, EngineSnapshot};
use mars_graph::build_routing_graph;
use mars_types::{
    CaptureConfig, ProcessTap, ProcessTapSelector, Profile, Route, RouteMatrix, VirtualInputDevice,
};

#[derive(Debug, Default)]
struct FakeTapIngress {
    samples: VecDeque<f32>,
    underrun_count: u64,
    xrun_count: u64,
}

impl FakeTapIngress {
    fn push_constant_block(&mut self, frames: usize, channels: usize, value: f32) {
        for _ in 0..frames.saturating_mul(channels) {
            self.samples.push_back(value);
        }
    }

    fn read_input_into(&mut self, out: &mut [f32]) {
        let mut underrun = false;
        for sample in out.iter_mut() {
            if let Some(value) = self.samples.pop_front() {
                *sample = value;
            } else {
                *sample = 0.0;
                underrun = true;
            }
        }
        if underrun {
            self.underrun_count = self.underrun_count.saturating_add(1);
            self.xrun_count = self.xrun_count.saturating_add(1);
        }
    }
}

fn capture_profile(frames: u32, channels: u16) -> Profile {
    let mut profile = Profile::default();
    profile.audio.buffer_frames = frames;
    profile.audio.channels = mars_types::AutoOrU16::Value(channels);
    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".to_string(),
        name: "Mix".to_string(),
        channels: Some(channels),
        uid: None,
        mix: None,
    });
    profile.captures = CaptureConfig {
        process_taps: vec![ProcessTap {
            id: "tap-app".to_string(),
            selector: ProcessTapSelector::Pid { pid: 4321 },
            channels: Some(channels),
        }],
        system_taps: Vec::new(),
    };
    profile.routes.push(Route {
        id: "route-tap-mix".to_string(),
        from: "tap-app".to_string(),
        to: "mix".to_string(),
        enabled: true,
        matrix: identity_matrix(channels),
        chain: None,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    profile
}

fn identity_matrix(channels: u16) -> RouteMatrix {
    let n = channels as usize;
    let mut coefficients = vec![vec![0.0; n]; n];
    for (index, row) in coefficients.iter_mut().enumerate().take(n) {
        row[index] = 1.0;
    }
    RouteMatrix {
        rows: channels,
        cols: channels,
        coefficients,
    }
}

fn bench_capture_render_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("daemon/capture_render/latency");
    for frames in [128_u32, 256_u32] {
        let channels = 2_u16;
        let profile = capture_profile(frames, channels);
        let graph = build_routing_graph(&profile).expect("build graph");
        let engine = Engine::new(EngineSnapshot {
            graph,
            sample_rate: 48_000,
            buffer_frames: frames,
        });
        let source_len = frames as usize * channels as usize;
        let mut source_buffers = HashMap::<String, Vec<f32>>::new();
        source_buffers.insert("tap-app".to_string(), vec![0.0; source_len]);
        let mut sink_buffers = HashMap::<String, Vec<f32>>::new();
        sink_buffers.insert("mix".to_string(), vec![0.0; source_len]);
        let mut ingress = FakeTapIngress::default();

        group.throughput(Throughput::Elements(frames as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{frames}f")),
            &frames,
            |b, _| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for index in 0..iters {
                        let token = (index % 1024) as f32 / 1024.0;
                        ingress.push_constant_block(frames as usize, channels as usize, token);
                        let Some(source) = source_buffers.get_mut("tap-app") else {
                            panic!("missing source buffer");
                        };
                        let started = Instant::now();
                        ingress.read_input_into(source);
                        sink_buffers.get_mut("mix").expect("sink").fill(0.0);
                        engine
                            .render_cycle_into(frames as usize, &source_buffers, &mut sink_buffers)
                            .expect("render");
                        let observed = sink_buffers
                            .get("mix")
                            .and_then(|samples| samples.first())
                            .copied()
                            .unwrap_or_default();
                        black_box(observed);
                        total += started.elapsed();
                    }
                    total
                });
            },
        );
    }
    group.finish();
}

fn bench_capture_render_xrun(c: &mut Criterion) {
    let mut group = c.benchmark_group("daemon/capture_render/xrun");
    let frames = 256_u32;
    let channels = 2_u16;
    let profile = capture_profile(frames, channels);
    let graph = build_routing_graph(&profile).expect("build graph");
    let engine = Engine::new(EngineSnapshot {
        graph,
        sample_rate: 48_000,
        buffer_frames: frames,
    });
    let source_len = frames as usize * channels as usize;
    let mut source_buffers = HashMap::<String, Vec<f32>>::new();
    source_buffers.insert("tap-app".to_string(), vec![0.0; source_len]);
    let mut sink_buffers = HashMap::<String, Vec<f32>>::new();
    sink_buffers.insert("mix".to_string(), vec![0.0; source_len]);
    let mut ingress = FakeTapIngress::default();

    group.throughput(Throughput::Elements(frames as u64));
    group.bench_function(BenchmarkId::from_parameter("fault25pct/256f"), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let before_xrun = ingress.xrun_count;
            for index in 0..iters {
                if index % 4 != 0 {
                    ingress.push_constant_block(frames as usize, channels as usize, 0.25);
                }
                let Some(source) = source_buffers.get_mut("tap-app") else {
                    panic!("missing source buffer");
                };
                let started = Instant::now();
                ingress.read_input_into(source);
                sink_buffers.get_mut("mix").expect("sink").fill(0.0);
                engine
                    .render_cycle_into(frames as usize, &source_buffers, &mut sink_buffers)
                    .expect("render");
                total += started.elapsed();
            }
            let delta_xrun = ingress.xrun_count.saturating_sub(before_xrun);
            assert!(delta_xrun > 0, "fault injection must produce xrun samples");
            black_box(delta_xrun);
            total
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_capture_render_latency,
    bench_capture_render_xrun
);
criterion_main!(benches);

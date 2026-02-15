#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU64, Ordering};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mars_hal::shm_backend::{RingRegistry, RingSpec};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_name(prefix: &str) -> String {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("mars.bench.{prefix}.{id}")
}

const SPEC: RingSpec = RingSpec {
    sample_rate: 48_000,
    channels: 2,
    capacity_frames: 4096,
};

fn bench_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring/write");

    for frames in [64, 256, 1024] {
        let samples: Vec<f32> = (0..frames * 2).map(|i| (i as f32 * 0.001).sin()).collect();

        group.bench_with_input(BenchmarkId::from_parameter(frames), &samples, |b, data| {
            let registry = RingRegistry::default();
            let name = unique_name("write");
            let handle = registry.create_or_open(&name, SPEC).unwrap();
            b.iter(|| {
                let mut ring = handle.lock();
                ring.write_interleaved(data).unwrap();
            });
            registry.remove(&name);
        });
    }

    group.finish();
}

fn bench_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring/read");

    for frames in [64, 256, 1024] {
        let samples: Vec<f32> = (0..frames * 2).map(|i| (i as f32 * 0.001).sin()).collect();

        group.bench_with_input(BenchmarkId::from_parameter(frames), &samples, |b, data| {
            let registry = RingRegistry::default();
            let name = unique_name("read");
            let handle = registry.create_or_open(&name, SPEC).unwrap();
            let mut out = vec![0.0_f32; data.len()];
            b.iter(|| {
                let mut ring = handle.lock();
                ring.write_interleaved(data).unwrap();
                ring.read_interleaved(&mut out).unwrap();
            });
            registry.remove(&name);
        });
    }

    group.finish();
}

fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("ring/roundtrip");

    for frames in [64, 256, 1024] {
        let samples: Vec<f32> = (0..frames * 2).map(|i| (i as f32 * 0.001).sin()).collect();

        group.bench_with_input(BenchmarkId::from_parameter(frames), &samples, |b, data| {
            let registry = RingRegistry::default();
            let name = unique_name("roundtrip");
            let handle = registry.create_or_open(&name, SPEC).unwrap();
            let mut out = vec![0.0_f32; data.len()];
            b.iter(|| {
                let mut ring = handle.lock();
                ring.write_interleaved(data).unwrap();
                ring.read_interleaved(&mut out).unwrap();
            });
            registry.remove(&name);
        });
    }

    group.finish();
}

criterion_group!(benches, bench_write, bench_read, bench_roundtrip);
criterion_main!(benches);

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_daemon::MarsDaemon;
use mars_ipc::{DaemonRequest, DaemonResponse, IpcClient, LogRequest};
use mars_shm::{RingSpec, StreamDirection, global_registry, stream_name};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

struct IpcBenchEnv {
    socket_path: PathBuf,
    log_path: PathBuf,
    server: JoinHandle<()>,
    client: IpcClient,
}

fn unique_tag(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{:x}", nanos % 0xFF_FFFF)
}

fn shm_uid(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}{:x}", nanos % 0xFFFF)
}

fn p95_micros(mut latencies_us: Vec<f64>) -> f64 {
    if latencies_us.is_empty() {
        return 0.0;
    }
    latencies_us.sort_by(|a, b| a.total_cmp(b));
    let idx = ((latencies_us.len() as f64) * 0.95).floor() as usize;
    latencies_us[idx.min(latencies_us.len() - 1)]
}

async fn start_ipc_env() -> IpcBenchEnv {
    let tag = unique_tag("daemon-ipc-bench");
    let socket_path = std::env::temp_dir().join(format!("{tag}.sock"));
    let log_path = std::env::temp_dir().join(format!("{tag}.log"));
    fs::write(&log_path, "one\ntwo\nthree\n").expect("seed log file");

    let daemon = Arc::new(MarsDaemon::new(log_path.clone()));
    let daemon_for_server = Arc::clone(&daemon);
    let socket_for_server = socket_path.clone();
    let server = tokio::spawn(async move {
        let _ = daemon_for_server.run(&socket_for_server).await;
    });

    for _ in 0..100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket_path.exists(), "daemon socket should be ready");

    let client = IpcClient::new(socket_path.clone(), Duration::from_millis(1_000));
    let response = client.send(DaemonRequest::Ping).await.expect("warmup ping");
    assert!(matches!(response, DaemonResponse::Pong));

    IpcBenchEnv {
        socket_path,
        log_path,
        server,
        client,
    }
}

async fn stop_ipc_env(env: IpcBenchEnv) {
    env.server.abort();
    let _ = env.server.await;
    let _ = tokio::fs::remove_file(&env.socket_path).await;
    let _ = tokio::fs::remove_file(&env.log_path).await;
}

async fn probe_ipc_status(client: &IpcClient, requests: usize) -> (f64, f64) {
    let mut latencies_us = Vec::with_capacity(requests);
    let started = Instant::now();

    for _ in 0..requests {
        let before = Instant::now();
        let response = client.send(DaemonRequest::Status).await.expect("status");
        assert!(matches!(response, DaemonResponse::Status(_)));
        latencies_us.push(before.elapsed().as_secs_f64() * 1_000_000.0);
    }

    let elapsed = started.elapsed().as_secs_f64();
    let req_per_sec = if elapsed > 0.0 {
        requests as f64 / elapsed
    } else {
        0.0
    };
    (req_per_sec, p95_micros(latencies_us))
}

fn probe_shm_roundtrip(frames: usize, iterations: usize) -> (f64, f64) {
    let uid = shm_uid("d");
    let name = stream_name(StreamDirection::Vout, &uid);
    let spec = RingSpec {
        sample_rate: 48_000,
        channels: 2,
        capacity_frames: (frames as u32).saturating_mul(8),
    };
    let ring = global_registry()
        .create_or_open(&name, spec)
        .expect("create ring");

    let mut payload = vec![0.25_f32; frames * 2];
    let mut sink = vec![0.0_f32; frames * 2];
    let mut latencies_us = Vec::with_capacity(iterations);
    let started = Instant::now();

    for i in 0..iterations {
        payload[0] = ((i as f32) * 0.01).sin();
        let before = Instant::now();
        ring.lock()
            .write_interleaved(&payload)
            .expect("write interleaved");
        let read = ring
            .lock()
            .read_interleaved(&mut sink)
            .expect("read interleaved");
        black_box(read);
        latencies_us.push(before.elapsed().as_secs_f64() * 1_000_000.0);
    }

    let elapsed = started.elapsed().as_secs_f64();
    let frames_per_sec = if elapsed > 0.0 {
        (frames as f64 * iterations as f64) / elapsed
    } else {
        0.0
    };
    let _ = global_registry().remove(&name);
    (frames_per_sec, p95_micros(latencies_us))
}

fn bench_daemon_ipc(c: &mut Criterion) {
    let runtime = Runtime::new().expect("tokio runtime");
    let env = runtime.block_on(start_ipc_env());
    let (req_per_sec, p95_us) = runtime.block_on(probe_ipc_status(&env.client, 2_000));
    println!("daemon/ipc_status probe: throughput={req_per_sec:.0} req/s p95={p95_us:.2}us");

    let ping_client = env.client.clone();
    let status_client = env.client.clone();
    let logs_client = env.client.clone();

    let mut group = c.benchmark_group("daemon/ipc");
    group.throughput(Throughput::Elements(1));
    group.bench_function("ping", |b| {
        b.iter(|| {
            let response = runtime
                .block_on(ping_client.send(DaemonRequest::Ping))
                .expect("ping");
            assert!(matches!(response, DaemonResponse::Pong));
        });
    });
    group.bench_function("status", |b| {
        b.iter(|| {
            let response = runtime
                .block_on(status_client.send(DaemonRequest::Status))
                .expect("status");
            assert!(matches!(response, DaemonResponse::Status(_)));
        });
    });
    group.bench_function("logs_tail", |b| {
        b.iter(|| {
            let response = runtime
                .block_on(logs_client.send(DaemonRequest::Logs(LogRequest {
                    follow: false,
                    cursor: None,
                    limit: Some(20),
                })))
                .expect("logs");
            let DaemonResponse::Logs(payload) = response else {
                panic!("expected logs response");
            };
            black_box(payload.next_cursor);
        });
    });
    group.finish();

    runtime.block_on(stop_ipc_env(env));
}

fn bench_shm_roundtrip(c: &mut Criterion) {
    let (frames_per_sec, p95_us) = probe_shm_roundtrip(256, 10_000);
    println!(
        "daemon/shm_roundtrip probe: throughput={frames_per_sec:.0} frames/s p95={p95_us:.2}us"
    );

    let mut group = c.benchmark_group("daemon/shm_roundtrip");
    for frames in [64_usize, 256, 1024] {
        let uid = shm_uid(match frames {
            64 => "a",
            256 => "b",
            _ => "c",
        });
        let name = stream_name(StreamDirection::Vout, &uid);
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 2,
            capacity_frames: (frames as u32).saturating_mul(8),
        };
        let ring = global_registry()
            .create_or_open(&name, spec)
            .expect("create ring");
        let mut payload = vec![0.25_f32; frames * 2];
        let mut sink = vec![0.0_f32; frames * 2];

        group.throughput(Throughput::Elements(frames as u64));
        group.bench_with_input(BenchmarkId::from_parameter(frames), &frames, |b, _| {
            b.iter(|| {
                payload[0] += 0.0001;
                ring.lock()
                    .write_interleaved(&payload)
                    .expect("write interleaved");
                let read = ring
                    .lock()
                    .read_interleaved(&mut sink)
                    .expect("read interleaved");
                black_box(read);
            });
        });

        let _ = global_registry().remove(&name);
    }
    group.finish();
}

criterion_group!(benches, bench_daemon_ipc, bench_shm_roundtrip);
criterion_main!(benches);

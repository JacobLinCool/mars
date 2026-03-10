#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_daemon::MarsDaemon;
use mars_ipc::{DaemonRequest, DaemonResponse, IpcClient, LogRequest};
use mars_shm::{RingSpec, StreamDirection, global_registry, stream_name};
use mars_types::{
    AuPluginApi, CaptureRuntimeHealth, CaptureRuntimeKind, CaptureRuntimeStatus,
    CaptureRuntimeTapStatus, DaemonStatus, DeviceDescriptor, DriverStatusSummary,
    ExternalRuntimeStatus, NodeKind, PluginHostHealth, PluginHostInstanceStatus,
    PluginHostRuntimeStatus, RuntimeCounters, SinkRuntimeHealth, SinkRuntimeKind,
    SinkRuntimeSinkStatus, SinkRuntimeStatus,
};
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

fn sample_status_payload() -> DaemonStatus {
    DaemonStatus {
        running: true,
        current_profile: Some("bench-profile".to_string()),
        sample_rate: 48_000,
        buffer_frames: 256,
        graph_pipe_count: 8,
        graph_route_count: 12,
        devices: vec![
            DeviceDescriptor {
                id: "app".to_string(),
                name: "App".to_string(),
                uid: "bench-vout".to_string(),
                kind: NodeKind::VirtualOutput,
                channels: 2,
                managed: true,
            },
            DeviceDescriptor {
                id: "mix".to_string(),
                name: "Mix".to_string(),
                uid: "bench-vin".to_string(),
                kind: NodeKind::VirtualInput,
                channels: 2,
                managed: true,
            },
        ],
        counters: RuntimeCounters {
            underrun_count: 1,
            overrun_count: 2,
            xrun_count: 3,
            deadline_miss_count: 4,
            last_callback_ns: 125_000,
            last_cycle_ns: 95_000,
            max_cycle_ns: 140_000,
        },
        processor_runtime: std::collections::BTreeMap::from([
            (
                "eq-main".to_string(),
                mars_types::ProcessorRuntimeStats {
                    prepare_calls: 1,
                    process_calls: 1024,
                    reset_calls: 0,
                    last_generation: 7,
                },
            ),
            (
                "dyn-main".to_string(),
                mars_types::ProcessorRuntimeStats {
                    prepare_calls: 1,
                    process_calls: 1024,
                    reset_calls: 0,
                    last_generation: 7,
                },
            ),
        ]),
        driver: DriverStatusSummary {
            generation: 7,
            request_count: 9,
            perform_count: 9,
            applied_device_count: 2,
            pending_change: false,
        },
        external_runtime: ExternalRuntimeStatus {
            connected_inputs: 1,
            connected_outputs: 1,
            degraded_inputs: 0,
            degraded_outputs: 0,
            restart_attempts: 0,
            stream_errors: Vec::new(),
        },
        capture_runtime: CaptureRuntimeStatus {
            supported: true,
            discovered_processes: 32,
            active_taps: 2,
            failed_taps: 0,
            taps: vec![CaptureRuntimeTapStatus {
                id: "tap-browser".to_string(),
                kind: CaptureRuntimeKind::ProcessTap,
                health: CaptureRuntimeHealth::Healthy,
                selector: "bundle_id:com.apple.Safari".to_string(),
                tap_id: Some(11),
                aggregate_uid: Some("bench.tap.browser".to_string()),
                aggregate_device_id: Some(99),
                matched_processes: 1,
                ingested_frames: 48_000,
                underrun_count: 0,
                overrun_count: 0,
                xrun_count: 0,
                restart_attempts: 0,
                error_ring: Vec::new(),
                last_error: None,
            }],
            errors: Vec::new(),
        },
        sink_runtime: SinkRuntimeStatus {
            queue_capacity: 128,
            queued_batches: 2,
            dropped_batches: 0,
            dropped_samples: 0,
            write_errors: 0,
            active_file_sinks: 1,
            active_stream_sinks: 1,
            sinks: vec![
                SinkRuntimeSinkStatus {
                    id: "record-main".to_string(),
                    source: "mix".to_string(),
                    kind: SinkRuntimeKind::File,
                    health: SinkRuntimeHealth::Healthy,
                    written_frames: 96_000,
                    dropped_batches: 0,
                    last_error: None,
                },
                SinkRuntimeSinkStatus {
                    id: "stream-main".to_string(),
                    source: "mix".to_string(),
                    kind: SinkRuntimeKind::Stream,
                    health: SinkRuntimeHealth::Healthy,
                    written_frames: 96_000,
                    dropped_batches: 0,
                    last_error: None,
                },
            ],
        },
        plugin_runtime: PluginHostRuntimeStatus {
            active_instances: 1,
            failed_instances: 0,
            timeout_count: 0,
            error_count: 0,
            restart_count: 0,
            instances: vec![PluginHostInstanceStatus {
                id: "au-main".to_string(),
                api: AuPluginApi::Auv2,
                health: PluginHostHealth::Healthy,
                loaded: true,
                host_pid: Some(1234),
                process_calls: 2048,
                timeout_count: 0,
                error_count: 0,
                restart_count: 0,
                last_error: None,
            }],
        },
        updated_at: Utc::now(),
    }
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

    let sample_status = sample_status_payload();
    let encoded_status = serde_json::to_vec(&sample_status).expect("status encode baseline");
    let mut serde_group = c.benchmark_group("daemon/ipc_serde");
    serde_group.throughput(Throughput::Bytes(encoded_status.len() as u64));
    serde_group.bench_function("status_encode", |b| {
        b.iter(|| {
            let encoded = serde_json::to_vec(black_box(&sample_status)).expect("status encode");
            black_box(encoded);
        });
    });
    serde_group.bench_function("status_decode", |b| {
        b.iter(|| {
            let decoded: DaemonStatus =
                serde_json::from_slice(black_box(&encoded_status)).expect("status decode");
            black_box(decoded);
        });
    });
    serde_group.finish();

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

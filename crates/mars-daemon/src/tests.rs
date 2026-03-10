use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mars_coreaudio::{ExternalEndpointHealth, ExternalInputEndpointSnapshot};
use mars_engine::{Engine, EngineSnapshot};
use mars_graph::build_routing_graph;
use mars_ipc::{DaemonRequest, DaemonResponse, IpcClient, LogRequest};
use mars_shm::{RingSpec, StreamDirection, global_registry, stream_name};
use mars_types::{
    AuPluginApi, AutoOrU32, CaptureRuntimeHealth, CaptureRuntimeKind, CaptureRuntimeStatus,
    CaptureRuntimeTapStatus, DeviceDescriptor, FileSink, FileSinkFormat, NodeKind, Pipe,
    PluginHostHealth, PluginHostInstanceStatus, PluginHostRuntimeStatus, ProcessTap,
    ProcessTapSelector, ProcessorChain, ProcessorDefinition, ProcessorKind, Profile, Route,
    RouteMatrix, SinkRuntimeHealth, SinkRuntimeKind, SinkRuntimeSinkStatus, SinkRuntimeStatus,
    SystemTap, SystemTapMode, VirtualInputDevice, VirtualOutputDevice,
};

use super::{
    MarsDaemon, collect_devices, diff_profiles, enrich_capture_runtime_with_external_inputs,
    is_driver_compatible,
};

fn temp_log_path(case: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("mars-daemon-{case}-{nanos}.log"))
}

fn temp_socket_path(case: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("mars-daemon-{case}-{nanos}.sock"))
}

#[test]
fn diff_detects_create_remove() {
    let mut before = Profile::default();
    before.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "old".to_string(),
        name: "Old".to_string(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });

    let mut after = Profile::default();
    after.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "new".to_string(),
        name: "New".to_string(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    after.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".to_string(),
        name: "Mix".to_string(),
        channels: Some(2),
        uid: None,
        mix: None,
    });
    after.pipes.push(Pipe {
        from: "new".to_string(),
        to: "mix".to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });

    let diff = diff_profiles(Some(&before), Some(&after));
    assert!(
        diff.changes
            .iter()
            .any(|change| matches!(change.kind, mars_types::PlanChangeKind::CreateDevice))
    );
    assert!(
        diff.changes
            .iter()
            .any(|change| matches!(change.kind, mars_types::PlanChangeKind::RemoveDevice))
    );
}

#[test]
fn clear_diff_when_none() {
    let before = Profile::default();
    let diff = diff_profiles(Some(&before), None);
    assert!(!diff.changes.is_empty());
}

#[test]
fn diff_detects_v2_routes_processors_captures_and_sinks() {
    let before = Profile::default();
    let mut after = Profile::default();

    after.routes.push(Route {
        id: "route-main".to_string(),
        from: "src".to_string(),
        to: "dst".to_string(),
        enabled: true,
        matrix: RouteMatrix {
            rows: 2,
            cols: 2,
            coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
        },
        chain: Some("chain-main".to_string()),
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    after.processors.push(ProcessorDefinition {
        id: "eq-main".to_string(),
        kind: ProcessorKind::Eq,
        config: serde_json::Value::Null,
    });
    after.processor_chains.push(ProcessorChain {
        id: "chain-main".to_string(),
        processors: vec!["eq-main".to_string()],
    });
    after.captures.process_taps.push(ProcessTap {
        id: "tap-main".to_string(),
        selector: ProcessTapSelector::Pid { pid: 1234 },
        channels: Some(2),
    });
    after.sinks.files.push(FileSink {
        id: "record-main".to_string(),
        source: "mix".to_string(),
        path: "/tmp/mars-record.wav".to_string(),
        format: FileSinkFormat::Wav,
        channels: Some(2),
    });

    let diff = diff_profiles(Some(&before), Some(&after));
    let targets = diff
        .changes
        .iter()
        .map(|change| change.target.as_str())
        .collect::<Vec<_>>();

    assert!(targets.contains(&"routes"));
    assert!(targets.contains(&"processor_chains"));
    assert!(targets.contains(&"captures"));
    assert!(targets.contains(&"sinks"));
}

#[test]
fn driver_compatibility_requires_install_and_load() {
    assert!(!is_driver_compatible(false, false, Some("0.1.0"), "0.1.0"));
    assert!(!is_driver_compatible(false, true, Some("0.1.0"), "0.1.0"));
    assert!(!is_driver_compatible(true, false, Some("0.1.0"), "0.1.0"));
    assert!(!is_driver_compatible(true, true, None, "0.1.0"));
    assert!(!is_driver_compatible(true, true, Some("2.1.0"), "1.4.0"));
    assert!(is_driver_compatible(true, true, Some("1.2.0"), "1.4.0"));
}

#[test]
fn apply_deadline_zero_disables_timeout() {
    let deadline = MarsDaemon::apply_deadline(0);
    assert!(MarsDaemon::ensure_within_deadline(deadline, "any-stage").is_ok());
}

#[test]
fn apply_deadline_reports_stage_timeout() {
    let deadline = MarsDaemon::apply_deadline(1);
    thread::sleep(Duration::from_millis(5));
    let error =
        MarsDaemon::ensure_within_deadline(deadline, "driver-stage").expect_err("timed out");
    assert!(error.message.contains("driver-stage"));
}

#[test]
fn logs_cursor_returns_incremental_lines() {
    let path = temp_log_path("incremental");
    fs::write(&path, "one\ntwo\nthree\n").expect("seed log");
    let daemon = MarsDaemon::new(path.clone());

    let initial = daemon
        .logs_internal(&LogRequest {
            follow: false,
            cursor: None,
            limit: Some(2),
        })
        .expect("initial logs");
    assert_eq!(initial.lines, vec!["two".to_string(), "three".to_string()]);

    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open append");
    writeln!(file, "four").expect("append four");
    writeln!(file, "five").expect("append five");

    let delta = daemon
        .logs_internal(&LogRequest {
            follow: true,
            cursor: Some(initial.next_cursor),
            limit: None,
        })
        .expect("delta logs");
    assert_eq!(delta.lines, vec!["four".to_string(), "five".to_string()]);
    assert!(delta.next_cursor > initial.next_cursor);

    let _ = fs::remove_file(path);
}

#[test]
fn logs_cursor_beyond_file_len_falls_back_to_tail() {
    let path = temp_log_path("cursor-fallback");
    fs::write(&path, "a\nb\nc\nd\n").expect("seed log");
    let daemon = MarsDaemon::new(path.clone());

    let result = daemon
        .logs_internal(&LogRequest {
            follow: false,
            cursor: Some(9_999),
            limit: Some(2),
        })
        .expect("fallback logs");
    assert_eq!(result.lines, vec!["c".to_string(), "d".to_string()]);

    let len = fs::metadata(&path).expect("metadata").len();
    assert_eq!(result.next_cursor, len);

    let _ = fs::remove_file(path);
}

#[test]
fn status_reports_sink_runtime_health_and_write_stats() {
    let mut profile = Profile::default();
    profile.audio.sample_rate = AutoOrU32::Value(48_000);
    profile.audio.buffer_frames = 64;
    profile.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app".to_string(),
        name: "App".to_string(),
        channels: Some(2),
        uid: Some("status-sink-vout".to_string()),
        hidden: false,
    });
    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".to_string(),
        name: "Mix".to_string(),
        channels: Some(2),
        uid: Some("status-sink-vin".to_string()),
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
    let sink_path = std::env::temp_dir().join(format!(
        "mars-daemon-status-sink-{}-{}.wav",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    profile.sinks.files.push(FileSink {
        id: "record-main".to_string(),
        source: "mix".to_string(),
        path: sink_path.display().to_string(),
        format: FileSinkFormat::Wav,
        channels: Some(2),
    });

    let graph = build_routing_graph(&profile).expect("graph");
    let engine = Arc::new(Engine::new(EngineSnapshot {
        graph: graph.clone(),
        sample_rate: 48_000,
        buffer_frames: 64,
    }));
    let daemon = MarsDaemon::new(temp_log_path("sink-status"));
    {
        let mut state = daemon.state.lock();
        state.current_profile = Some(profile.clone());
        state.graph = Some(graph);
        state.engine = Some(engine);
        state.devices = vec![
            DeviceDescriptor {
                id: "app".to_string(),
                name: "App".to_string(),
                uid: "status-sink-vout".to_string(),
                kind: NodeKind::VirtualOutput,
                channels: 2,
                managed: true,
            },
            DeviceDescriptor {
                id: "mix".to_string(),
                name: "Mix".to_string(),
                uid: "status-sink-vin".to_string(),
                kind: NodeKind::VirtualInput,
                channels: 2,
                managed: true,
            },
        ];
    }

    daemon.sync_render_runtime().expect("start runtime");
    let ring_name = stream_name(StreamDirection::Vout, "status-sink-vout");
    let ring_spec = RingSpec {
        sample_rate: 48_000,
        channels: 2,
        capacity_frames: 64 * 8,
    };
    let ring = global_registry()
        .create_or_open(&ring_name, ring_spec)
        .expect("open vout ring");
    let source = vec![0.4_f32; 64 * 2];
    for _ in 0..32 {
        ring.lock()
            .write_interleaved(&source)
            .expect("write source");
        thread::sleep(Duration::from_millis(2));
    }
    thread::sleep(Duration::from_millis(40));

    let status = daemon.status_internal();
    assert_eq!(status.sink_runtime.sinks.len(), 1);
    assert_eq!(status.sink_runtime.sinks[0].id, "record-main");
    assert!(status.sink_runtime.sinks[0].written_frames > 0);
    assert_eq!(status.sink_runtime.write_errors, 0);

    daemon.stop_render_runtime();
    let _ = fs::remove_file(sink_path);
}

#[test]
fn doctor_report_includes_sink_runtime_health_summary() {
    let daemon = MarsDaemon::new(temp_log_path("doctor-sink-health"));
    {
        let mut state = daemon.state.lock();
        state.sink_runtime = SinkRuntimeStatus {
            queue_capacity: 64,
            queued_batches: 1,
            dropped_batches: 2,
            dropped_samples: 512,
            write_errors: 3,
            active_file_sinks: 1,
            active_stream_sinks: 1,
            sinks: vec![
                SinkRuntimeSinkStatus {
                    id: "record-main".to_string(),
                    source: "mix".to_string(),
                    kind: SinkRuntimeKind::File,
                    health: SinkRuntimeHealth::Degraded,
                    written_frames: 1024,
                    dropped_batches: 1,
                    last_error: Some("disk slow".to_string()),
                },
                SinkRuntimeSinkStatus {
                    id: "stream-main".to_string(),
                    source: "mix".to_string(),
                    kind: SinkRuntimeKind::Stream,
                    health: SinkRuntimeHealth::Failed,
                    written_frames: 0,
                    dropped_batches: 2,
                    last_error: Some("disconnected".to_string()),
                },
            ],
        };
    }

    let report = daemon.doctor_report_internal();
    assert_eq!(report.sink_active, 2);
    assert_eq!(report.sink_degraded, 1);
    assert_eq!(report.sink_failed, 1);
    assert_eq!(report.sink_write_errors, 3);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("sink runtime health"))
    );
}

#[test]
fn doctor_report_includes_plugin_runtime_health_summary() {
    let daemon = MarsDaemon::new(temp_log_path("doctor-plugin-health"));
    {
        let mut state = daemon.state.lock();
        state.plugin_runtime = PluginHostRuntimeStatus {
            active_instances: 1,
            failed_instances: 1,
            timeout_count: 2,
            error_count: 3,
            restart_count: 4,
            instances: vec![
                PluginHostInstanceStatus {
                    id: "au-main".to_string(),
                    api: AuPluginApi::Auv2,
                    health: PluginHostHealth::Healthy,
                    loaded: true,
                    host_pid: Some(101),
                    process_calls: 50,
                    timeout_count: 0,
                    error_count: 0,
                    restart_count: 0,
                    last_error: None,
                },
                PluginHostInstanceStatus {
                    id: "au-failed".to_string(),
                    api: AuPluginApi::Auv3,
                    health: PluginHostHealth::Failed,
                    loaded: false,
                    host_pid: None,
                    process_calls: 12,
                    timeout_count: 2,
                    error_count: 3,
                    restart_count: 4,
                    last_error: Some("plugin host crashed".to_string()),
                },
            ],
        };
    }

    let report = daemon.doctor_report_internal();
    assert_eq!(report.plugin_active, 1);
    assert_eq!(report.plugin_failed, 1);
    assert_eq!(report.plugin_timeouts, 2);
    assert_eq!(report.plugin_errors, 3);
    assert_eq!(report.plugin_restarts, 4);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("plugin runtime health"))
    );
}

#[test]
fn collect_devices_includes_capture_tap_external_inputs() {
    let mut profile = Profile::default();
    profile.captures.process_taps.push(ProcessTap {
        id: "tap-app".to_string(),
        selector: ProcessTapSelector::Pid { pid: 3333 },
        channels: Some(2),
    });
    profile.captures.system_taps.push(SystemTap {
        id: "tap-system".to_string(),
        mode: SystemTapMode::AllOutput,
        channels: Some(2),
    });

    let devices = collect_devices(&profile, &[]);
    assert!(
        devices
            .iter()
            .any(|device| device.id == "tap-app" && device.kind == NodeKind::ExternalInput)
    );
    assert!(
        devices
            .iter()
            .any(|device| device.id == "tap-system" && device.kind == NodeKind::ExternalInput)
    );
}

#[test]
fn capture_runtime_status_merges_live_external_input_metrics() {
    let status = CaptureRuntimeStatus {
        supported: true,
        discovered_processes: 1,
        active_taps: 1,
        failed_taps: 0,
        taps: vec![CaptureRuntimeTapStatus {
            id: "tap-app".to_string(),
            kind: CaptureRuntimeKind::ProcessTap,
            health: CaptureRuntimeHealth::Healthy,
            selector: "pid:3333".to_string(),
            tap_id: Some(42),
            aggregate_uid: Some("agg.tap-app".to_string()),
            aggregate_device_id: Some(77),
            matched_processes: 1,
            ingested_frames: 0,
            underrun_count: 0,
            overrun_count: 0,
            xrun_count: 0,
            restart_attempts: 0,
            error_ring: Vec::new(),
            last_error: None,
        }],
        errors: Vec::new(),
    };

    let merged = enrich_capture_runtime_with_external_inputs(
        status,
        &[ExternalInputEndpointSnapshot {
            node_id: "tap-app".to_string(),
            uid: "agg.tap-app".to_string(),
            health: ExternalEndpointHealth::Degraded,
            ingested_frames: 1024,
            underrun_count: 3,
            overrun_count: 1,
            xrun_count: 4,
            restart_attempts: 2,
            error_ring: vec!["reconnect failed".to_string()],
        }],
    );

    assert_eq!(merged.taps.len(), 1);
    let tap = &merged.taps[0];
    assert_eq!(tap.health, CaptureRuntimeHealth::Degraded);
    assert_eq!(tap.ingested_frames, 1024);
    assert_eq!(tap.underrun_count, 3);
    assert_eq!(tap.overrun_count, 1);
    assert_eq!(tap.xrun_count, 4);
    assert_eq!(tap.restart_attempts, 2);
    assert_eq!(tap.last_error.as_deref(), Some("reconnect failed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_ipc_shm_soak_survives_ring_churn_and_reports_deadline_pressure() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let short = format!("{:x}", suffix % 0xFFFFF);
    let vout_uid = format!("v{short}");
    let vin_uid = format!("i{short}");

    let mut profile = Profile::default();
    profile.audio.sample_rate = AutoOrU32::Value(384_000);
    profile.audio.buffer_frames = 16;
    profile.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app".to_string(),
        name: "App".to_string(),
        channels: Some(2),
        uid: Some(vout_uid.clone()),
        hidden: false,
    });
    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".to_string(),
        name: "Mix".to_string(),
        channels: Some(2),
        uid: Some(vin_uid.clone()),
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

    let graph = build_routing_graph(&profile).expect("graph");
    let engine = Arc::new(Engine::new(EngineSnapshot {
        graph: graph.clone(),
        sample_rate: 384_000,
        buffer_frames: 16,
    }));

    let devices = vec![
        DeviceDescriptor {
            id: "app".to_string(),
            name: "App".to_string(),
            uid: vout_uid.clone(),
            kind: NodeKind::VirtualOutput,
            channels: 2,
            managed: true,
        },
        DeviceDescriptor {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            uid: vin_uid.clone(),
            kind: NodeKind::VirtualInput,
            channels: 2,
            managed: true,
        },
    ];

    let log_path = temp_log_path("ipc-shm-soak");
    fs::write(&log_path, "boot\nready\n").expect("seed log file");
    let daemon = Arc::new(MarsDaemon::new(log_path.clone()));
    {
        let mut state = daemon.state.lock();
        state.current_profile_path = Some("in-memory-profile".to_string());
        state.current_profile = Some(profile.clone());
        state.graph = Some(graph);
        state.engine = Some(engine);
        state.devices = devices;
    }
    daemon.sync_render_runtime().expect("start render runtime");

    let socket_path = temp_socket_path("ipc-shm-soak");
    let daemon_for_server = Arc::clone(&daemon);
    let socket_for_server = socket_path.clone();
    let server = tokio::spawn(async move {
        let _ = daemon_for_server.run(&socket_for_server).await;
    });

    for _ in 0..80 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket_path.exists());

    let client = IpcClient::new(socket_path.clone(), Duration::from_millis(500));

    let ping = client.send(DaemonRequest::Ping).await.expect("ping");
    assert!(matches!(ping, DaemonResponse::Pong));

    let logs = client
        .send(DaemonRequest::Logs(LogRequest {
            follow: false,
            cursor: None,
            limit: Some(2),
        }))
        .await
        .expect("logs request");
    assert!(matches!(
        logs,
        DaemonResponse::Logs(response)
        if response.lines == vec!["boot".to_string(), "ready".to_string()]
    ));

    let ring_spec = RingSpec {
        sample_rate: 384_000,
        channels: 2,
        capacity_frames: 128,
    };
    let vout_name = stream_name(StreamDirection::Vout, &vout_uid);
    let vin_name = stream_name(StreamDirection::Vin, &vin_uid);

    let mut saw_sink_frames = false;
    tokio::time::sleep(Duration::from_millis(30)).await;
    for cycle in 0..220_u32 {
        let vout = global_registry()
            .create_or_open(&vout_name, ring_spec)
            .expect("open vout ring");
        let source = [0.25_f32, -0.25_f32].repeat(16);
        if cycle % 25 == 0 {
            let _ = vout.lock().write_interleaved(&[0.25_f32]);
        }
        vout.lock()
            .write_interleaved(&source)
            .expect("write source sample");

        tokio::time::sleep(Duration::from_millis(2)).await;

        let vin = global_registry()
            .create_or_open(&vin_name, ring_spec)
            .expect("open vin ring");
        let mut rendered = vec![0.0_f32; 32];
        let read_frames = vin
            .lock()
            .read_interleaved(&mut rendered)
            .expect("read sink sample");
        if read_frames > 0 {
            saw_sink_frames = true;
        }

        if cycle % 40 == 0 {
            let status = client.send(DaemonRequest::Status).await.expect("status");
            let DaemonResponse::Status(payload) = status else {
                panic!("expected status response");
            };
            assert!(payload.running);
            assert_eq!(payload.sample_rate, 384_000);
            assert_eq!(payload.buffer_frames, 16);
        }
    }

    assert!(saw_sink_frames);
    let final_status = client
        .send(DaemonRequest::Status)
        .await
        .expect("final status");
    let DaemonResponse::Status(payload) = final_status else {
        panic!("expected final status response");
    };
    assert!(
        payload.counters.deadline_miss_count > 0,
        "fault injection should produce deadline pressure"
    );

    daemon.stop_render_runtime();
    server.abort();
    let _ = server.await;
    let _ = global_registry().remove(&vout_name);
    let _ = global_registry().remove(&vin_name);
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&log_path);
}

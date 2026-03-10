use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mars_engine::{Engine, EngineSnapshot};
use mars_graph::build_routing_graph;
use mars_types::{
    ProcessorChain, ProcessorDefinition, ProcessorKind, Profile, Route, RouteMatrix,
    VirtualInputDevice, VirtualOutputDevice,
};
use serde_json::json;

fn unique_temp_path(tag: &str, ext: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mars-au-bench-{tag}-{ts}-{}.{}",
        std::process::id(),
        ext
    ))
}

fn write_mock_plugin_host_script() -> PathBuf {
    let script_path = unique_temp_path("mock-plugin-host", "py");
    let script = r#"#!/usr/bin/env python3
import json
import os
import socket
import sys

def send(conn, obj):
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))

def parse_socket(argv):
    for i, arg in enumerate(argv):
        if arg == "--socket" and i + 1 < len(argv):
            return argv[i + 1]
    raise RuntimeError("missing --socket")

def main():
    socket_path = parse_socket(sys.argv[1:])
    if os.path.exists(socket_path):
        os.remove(socket_path)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(socket_path)
    listener.listen(1)
    conn, _ = listener.accept()
    instances = {}
    buffer = b""
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            break
        buffer += chunk
        while b"\n" in buffer:
            line, buffer = buffer.split(b"\n", 1)
            if not line:
                continue
            request = json.loads(line.decode("utf-8"))
            kind = request["kind"]
            if kind == "handshake":
                send(conn, {"kind": "handshake", "protocol_version": 1})
            elif kind == "load":
                instances[request["instance_id"]] = {"prepared": False}
                send(conn, {"kind": "ack"})
            elif kind == "prepare":
                send(conn, {"kind": "ack"})
            elif kind == "process":
                send(conn, {"kind": "processed", "samples": request["samples"]})
            elif kind == "reset":
                send(conn, {"kind": "ack"})
            elif kind == "unload":
                instances.pop(request["instance_id"], None)
                send(conn, {"kind": "ack"})
            elif kind == "shutdown":
                send(conn, {"kind": "ack"})
                conn.close()
                listener.close()
                if os.path.exists(socket_path):
                    os.remove(socket_path)
                return
            else:
                send(conn, {"kind": "error", "message": "unknown request"})

if __name__ == "__main__":
    main()
"#;
    fs::write(&script_path, script).expect("write mock plugin host script");
    script_path
}

fn build_au_engine(script_path: &PathBuf, frames: u32) -> Engine {
    let mut profile = Profile::default();
    profile.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "src".to_string(),
        name: "Src".to_string(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "sink".to_string(),
        name: "Sink".to_string(),
        channels: Some(2),
        uid: None,
        mix: None,
    });
    profile.processors.push(ProcessorDefinition {
        id: "au-main".to_string(),
        kind: ProcessorKind::Au,
        config: json!({
            "api": "auv2",
            "component_type": "aufx",
            "component_subtype": "gain",
            "component_manufacturer": "appl",
            "process_timeout_ms": 25,
            "max_frames": 2048,
            "host_command": "python3",
            "host_args": [script_path.to_string_lossy().to_string()],
        }),
    });
    profile.processor_chains.push(ProcessorChain {
        id: "chain-au".to_string(),
        processors: vec!["au-main".to_string()],
    });
    profile.routes.push(Route {
        id: "route-au".to_string(),
        from: "src".to_string(),
        to: "sink".to_string(),
        enabled: true,
        matrix: RouteMatrix {
            rows: 2,
            cols: 2,
            coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
        },
        chain: Some("chain-au".to_string()),
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });

    let graph = build_routing_graph(&profile).expect("graph");
    Engine::new(EngineSnapshot {
        graph,
        sample_rate: 48_000,
        buffer_frames: frames,
    })
}

fn bench_plugin_host(c: &mut Criterion) {
    let script_path = write_mock_plugin_host_script();
    let frames = 256usize;
    let engine = build_au_engine(&script_path, frames as u32);

    let mut sources = HashMap::new();
    let input = (0..(frames * 2))
        .map(|index| ((index as f32) * 0.01).sin())
        .collect::<Vec<_>>();
    sources.insert("src".to_string(), input);

    for _ in 0..16 {
        engine.render_cycle(frames, &sources).expect("warm render");
        thread::sleep(Duration::from_millis(2));
    }

    let mut overhead = c.benchmark_group("engine/au/ipc_overhead");
    overhead.bench_function(BenchmarkId::new("render_submit", frames), |b| {
        b.iter(|| {
            engine.render_cycle(frames, &sources).expect("render");
        })
    });
    overhead.finish();

    let mut latency = c.benchmark_group("engine/au/plugin_latency");
    latency.bench_function(BenchmarkId::new("process_roundtrip", frames), |b| {
        let mut expected_calls = engine
            .plugin_runtime_status()
            .instances
            .first()
            .map_or(0, |status| status.process_calls);
        b.iter_custom(|iterations| {
            let started = Instant::now();
            for _ in 0..iterations {
                expected_calls = expected_calls.saturating_add(1);
                engine.render_cycle(frames, &sources).expect("render");
                let deadline = Instant::now() + Duration::from_millis(100);
                loop {
                    let calls = engine
                        .plugin_runtime_status()
                        .instances
                        .first()
                        .map_or(0, |status| status.process_calls);
                    if calls >= expected_calls || Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
            started.elapsed()
        })
    });
    latency.finish();

    let _ = fs::remove_file(script_path);
}

criterion_group!(benches, bench_plugin_host);
criterion_main!(benches);

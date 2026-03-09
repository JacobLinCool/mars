use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_daemon::sink_runtime::{SinkBinding, SinkBindingKind, SinkRuntime};
use mars_types::FileSinkFormat;

fn temp_sink_path(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir()
        .join(format!("mars-bench-{tag}-{nanos}.wav"))
        .display()
        .to_string()
}

fn rendered_sinks(sink_count: usize, frames: usize, channels: usize) -> HashMap<String, Vec<f32>> {
    let mut map = HashMap::new();
    for index in 0..sink_count {
        let samples = (0..frames * channels)
            .map(|sample| ((sample as f32) * 0.001 + index as f32).sin())
            .collect::<Vec<_>>();
        map.insert(format!("mix-{index}"), samples);
    }
    map
}

fn bench_sink_submit(c: &mut Criterion) {
    let mut group = c.benchmark_group("daemon/sink/submit");
    let frames = 256usize;
    let channels = 2usize;

    for sink_count in [1usize, 4usize] {
        let mut bindings = Vec::new();
        let mut paths = Vec::new();
        for index in 0..sink_count {
            let path = temp_sink_path(&format!("submit-{sink_count}-{index}"));
            paths.push(path.clone());
            bindings.push(SinkBinding {
                id: format!("sink-{index}"),
                source: format!("mix-{index}"),
                channels: channels as u16,
                kind: SinkBindingKind::File {
                    path,
                    format: FileSinkFormat::Wav,
                },
            });
        }

        let runtime = SinkRuntime::start(bindings, 48_000, 256).expect("sink runtime");
        let submitter = runtime.submitter();
        let rendered = rendered_sinks(sink_count, frames, channels);

        group.throughput(Throughput::Elements(
            (frames * channels * sink_count) as u64,
        ));
        group.bench_with_input(
            BenchmarkId::new(format!("{sink_count}sinks"), frames),
            &(frames, &rendered),
            |b, (render_frames, rendered_sinks)| {
                b.iter(|| submitter.submit_rendered_sinks(rendered_sinks, *render_frames));
            },
        );

        let _ = runtime.status();
        runtime.stop();
        for path in paths {
            let _ = fs::remove_file(path);
        }
    }

    group.finish();
}

fn bench_sink_backpressure(c: &mut Criterion) {
    let mut group = c.benchmark_group("daemon/sink/backpressure");
    let frames = 256usize;
    let channels = 2usize;

    let path = temp_sink_path("backpressure");
    let bindings = vec![SinkBinding {
        id: "sink-main".to_string(),
        source: "mix-main".to_string(),
        channels: channels as u16,
        kind: SinkBindingKind::File {
            path: path.clone(),
            format: FileSinkFormat::Wav,
        },
    }];
    let runtime = SinkRuntime::start(bindings, 48_000, 4).expect("sink runtime");
    let submitter = runtime.submitter();
    let rendered = rendered_sinks(1, frames, channels);

    group.throughput(Throughput::Elements((frames * channels) as u64));
    group.bench_function(BenchmarkId::new("queue4", frames), |b| {
        b.iter(|| submitter.submit_rendered_sinks(&rendered, frames));
    });

    let _ = runtime.status();
    runtime.stop();
    let _ = fs::remove_file(path);
    group.finish();
}

criterion_group!(benches, bench_sink_submit, bench_sink_backpressure);
criterion_main!(benches);

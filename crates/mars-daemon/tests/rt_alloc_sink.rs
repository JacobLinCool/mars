#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mars_daemon::sink_runtime::{SinkBinding, SinkBindingKind, SinkRuntime};
use mars_types::FileSinkFormat;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<std::alloc::System> = &INSTRUMENTED_SYSTEM;

fn temp_file_path(ext: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mars-rt-alloc-sink-{ts}-{}.{}",
        std::process::id(),
        ext
    ))
}

#[test]
fn submit_rendered_sinks_has_zero_heap_allocation_steady_state() {
    let frames = 256usize;
    let channels = 2usize;
    let path = temp_file_path("wav");
    let runtime = SinkRuntime::start(
        vec![SinkBinding {
            id: "sink-main".to_string(),
            source: "mix".to_string(),
            channels: channels as u16,
            kind: SinkBindingKind::File {
                path: path.to_string_lossy().to_string(),
                format: FileSinkFormat::Wav,
            },
        }],
        48_000,
        frames,
        64,
    )
    .expect("sink runtime");
    let submitter = runtime.submitter();
    let mut rendered = HashMap::new();
    rendered.insert("mix".to_string(), vec![0.25_f32; frames * channels]);

    // Warm up the submit path, the worker, and lazy one-time runtime
    // internals (channel parker, writer scratch buffers, pooled buffers).
    for _ in 0..128 {
        submitter.submit_rendered_sinks(&rendered, frames);
    }
    for _ in 0..200 {
        if runtime.status().queued_batches == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    let region = Region::new(GLOBAL);
    submitter.submit_rendered_sinks(&rendered, frames);
    let delta = region.change();

    assert_eq!(delta.allocations, 0);
    assert_eq!(delta.reallocations, 0);
    assert_eq!(delta.deallocations, 0);

    runtime.stop();
    let _ = fs::remove_file(path);
}

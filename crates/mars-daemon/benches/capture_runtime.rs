use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_audio_tap::{
    AudioProcessInfo, ProcessTapRequest, SystemTapRequest, SystemTapTarget, TapCapability,
    TapHandle,
};
use mars_daemon::capture_runtime::{CaptureRuntime, TapBackend};
use mars_types::{
    CaptureConfig, ProcessTap, ProcessTapSelector, Profile, SystemTap, SystemTapMode,
};

#[derive(Debug)]
struct BenchTapBackend {
    processes: Vec<AudioProcessInfo>,
    next_tap_id: AtomicU32,
    created: Mutex<u64>,
    destroyed: Mutex<u64>,
}

impl BenchTapBackend {
    fn new(processes: Vec<AudioProcessInfo>) -> Self {
        Self {
            processes,
            next_tap_id: AtomicU32::new(1000),
            created: Mutex::new(0),
            destroyed: Mutex::new(0),
        }
    }
}

impl TapBackend for BenchTapBackend {
    fn capability(&self) -> Result<TapCapability, String> {
        Ok(TapCapability {
            supported: true,
            reason: None,
        })
    }

    fn list_processes(&self) -> Result<Vec<AudioProcessInfo>, String> {
        Ok(self.processes.clone())
    }

    fn create_process_tap(&self, request: &ProcessTapRequest) -> Result<TapHandle, String> {
        let tap_id = self.next_tap_id.fetch_add(1, Ordering::Relaxed);
        *self.created.lock().expect("lock created") += 1;
        Ok(TapHandle {
            tap_id,
            tap_uid: format!("bench.tap.process.{tap_id}"),
            aggregate_device_id: tap_id + 10_000,
            aggregate_uid: request.aggregate_uid.clone(),
        })
    }

    fn create_system_tap(&self, request: &SystemTapRequest) -> Result<TapHandle, String> {
        let tap_id = self.next_tap_id.fetch_add(1, Ordering::Relaxed);
        *self.created.lock().expect("lock created") += 1;
        let target = match request.target {
            SystemTapTarget::DefaultOutput => "default",
            SystemTapTarget::AllOutput => "all",
        };
        Ok(TapHandle {
            tap_id,
            tap_uid: format!("bench.tap.system.{target}.{tap_id}"),
            aggregate_device_id: tap_id + 20_000,
            aggregate_uid: request.aggregate_uid.clone(),
        })
    }

    fn destroy_tap(&self, _handle: &TapHandle) -> Result<(), String> {
        *self.destroyed.lock().expect("lock destroyed") += 1;
        Ok(())
    }
}

fn process_profile(process_tap_count: usize) -> Profile {
    let mut profile = Profile::default();
    let mut process_taps = Vec::new();
    for index in 0..process_tap_count {
        process_taps.push(ProcessTap {
            id: format!("tap-app-{index}"),
            selector: ProcessTapSelector::BundleId {
                bundle_id: format!("com.example.app{index}"),
            },
            channels: Some(2),
        });
    }
    profile.captures = CaptureConfig {
        process_taps,
        system_taps: vec![SystemTap {
            id: "tap-system".to_string(),
            mode: SystemTapMode::AllOutput,
            channels: Some(2),
        }],
    };
    profile
}

fn process_list(count: usize) -> Vec<AudioProcessInfo> {
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        out.push(AudioProcessInfo {
            process_object_id: (index + 1) as u32,
            pid: (10_000 + index) as i32,
            bundle_id: format!("com.example.app{index}"),
            is_running: true,
            is_running_input: false,
            is_running_output: true,
        });
    }
    out
}

fn bench_capture_create_destroy(c: &mut Criterion) {
    let backend = Arc::new(BenchTapBackend::new(process_list(1)));
    let profile = process_profile(1);
    let mut group = c.benchmark_group("daemon/capture/create_destroy");
    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::from_parameter("1tap"), |b| {
        b.iter(|| {
            let runtime = CaptureRuntime::start_with_backend(&profile, backend.clone())
                .expect("start runtime");
            runtime.stop();
        });
    });
    group.finish();
}

fn bench_capture_control_plane(c: &mut Criterion) {
    let mut group = c.benchmark_group("daemon/capture/control_plane");
    for tap_count in [4usize, 8usize, 16usize] {
        let profile = process_profile(tap_count);
        let backend = Arc::new(BenchTapBackend::new(process_list(tap_count)));

        group.throughput(Throughput::Elements(tap_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{tap_count}taps")),
            &tap_count,
            |b, _| {
                b.iter(|| {
                    let runtime = CaptureRuntime::start_with_backend(&profile, backend.clone())
                        .expect("start runtime");
                    runtime.stop();
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_capture_create_destroy,
    bench_capture_control_plane
);
criterion_main!(benches);

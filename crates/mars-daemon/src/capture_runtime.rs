use std::sync::Arc;

use mars_audio_tap::{
    AudioProcessInfo, CoreAudioTapController, ProcessTapRequest, SystemTapRequest, SystemTapTarget,
    TapCapability, TapHandle, TapMuteBehavior,
};
use mars_types::{
    CaptureRuntimeHealth, CaptureRuntimeKind, CaptureRuntimeStatus, CaptureRuntimeTapStatus,
    ProcessTapSelector, Profile, SystemTapMode,
};

#[derive(Debug)]
struct ActiveCaptureTap {
    id: String,
    handle: TapHandle,
}

pub trait TapBackend: Send + Sync {
    fn capability(&self) -> Result<TapCapability, String>;
    fn list_processes(&self) -> Result<Vec<AudioProcessInfo>, String>;
    fn create_process_tap(&self, request: &ProcessTapRequest) -> Result<TapHandle, String>;
    fn create_system_tap(&self, request: &SystemTapRequest) -> Result<TapHandle, String>;
    fn destroy_tap(&self, handle: &TapHandle) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct CoreAudioTapBackend {
    controller: CoreAudioTapController,
}

impl TapBackend for CoreAudioTapBackend {
    fn capability(&self) -> Result<TapCapability, String> {
        self.controller
            .capability()
            .map_err(|error| error.to_string())
    }

    fn list_processes(&self) -> Result<Vec<AudioProcessInfo>, String> {
        self.controller
            .list_processes()
            .map_err(|error| error.to_string())
    }

    fn create_process_tap(&self, request: &ProcessTapRequest) -> Result<TapHandle, String> {
        self.controller
            .create_process_tap(request)
            .map_err(|error| error.to_string())
    }

    fn create_system_tap(&self, request: &SystemTapRequest) -> Result<TapHandle, String> {
        self.controller
            .create_system_tap(request)
            .map_err(|error| error.to_string())
    }

    fn destroy_tap(&self, handle: &TapHandle) -> Result<(), String> {
        self.controller
            .destroy_tap(handle)
            .map_err(|error| error.to_string())
    }
}

pub struct CaptureRuntime {
    backend: Arc<dyn TapBackend>,
    active: Vec<ActiveCaptureTap>,
    status: CaptureRuntimeStatus,
}

impl std::fmt::Debug for CaptureRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureRuntime")
            .field("active_taps", &self.active.len())
            .field("status", &self.status)
            .finish()
    }
}

impl CaptureRuntime {
    pub fn start(profile: &Profile) -> Result<Self, String> {
        Self::start_with_backend(profile, Arc::new(CoreAudioTapBackend::default()))
    }

    pub fn start_with_backend(
        profile: &Profile,
        backend: Arc<dyn TapBackend>,
    ) -> Result<Self, String> {
        let configured_taps =
            profile.captures.process_taps.len() + profile.captures.system_taps.len();

        let capability = backend
            .capability()
            .map_err(|error| format!("tap capability check failed: {error}"))?;

        if !capability.supported {
            if configured_taps == 0 {
                let mut status = CaptureRuntimeStatus {
                    supported: false,
                    ..CaptureRuntimeStatus::default()
                };
                if let Some(reason) = capability.reason {
                    status.errors.push(reason);
                }
                return Ok(Self {
                    backend,
                    active: Vec::new(),
                    status,
                });
            }
            let reason = capability
                .reason
                .unwrap_or_else(|| "tap API is unavailable on this host".to_string());
            return Err(format!("capture taps are unsupported: {reason}"));
        }

        let mut processes = backend
            .list_processes()
            .map_err(|error| format!("failed to enumerate CoreAudio process objects: {error}"))?;
        processes.sort_by_key(|process| (process.pid, process.process_object_id));

        let mut active = Vec::<ActiveCaptureTap>::new();
        let mut tap_status = Vec::<CaptureRuntimeTapStatus>::new();

        for tap in &profile.captures.process_taps {
            let matched = resolve_process_selector(&tap.selector, &processes);
            if matched.is_empty() {
                cleanup_handles(&backend, &active);
                return Err(format!(
                    "process tap '{}' selector '{}' did not match any active CoreAudio process object",
                    tap.id,
                    selector_string(&tap.selector)
                ));
            }

            let process_object_ids = matched
                .iter()
                .map(|item| item.process_object_id)
                .collect::<Vec<_>>();
            let request = ProcessTapRequest {
                process_object_ids,
                tap_name: format!("mars process tap {}", tap.id),
                aggregate_uid: capture_aggregate_uid(&tap.id),
                aggregate_name: aggregate_name(&tap.id),
                mono: tap.channels == Some(1),
                private_tap: true,
                mute_behavior: TapMuteBehavior::Unmuted,
                auto_start: true,
            };

            let handle = match backend.create_process_tap(&request) {
                Ok(handle) => handle,
                Err(error) => {
                    cleanup_handles(&backend, &active);
                    return Err(format!(
                        "failed to create process tap '{}': {error}",
                        tap.id
                    ));
                }
            };

            active.push(ActiveCaptureTap {
                id: tap.id.clone(),
                handle: handle.clone(),
            });
            tap_status.push(CaptureRuntimeTapStatus {
                id: tap.id.clone(),
                kind: CaptureRuntimeKind::ProcessTap,
                health: CaptureRuntimeHealth::Healthy,
                selector: selector_string(&tap.selector),
                tap_id: Some(handle.tap_id),
                aggregate_uid: Some(handle.aggregate_uid.clone()),
                aggregate_device_id: Some(handle.aggregate_device_id),
                matched_processes: matched.len(),
                last_error: None,
                ..CaptureRuntimeTapStatus::default()
            });
        }

        for tap in &profile.captures.system_taps {
            let request = SystemTapRequest {
                target: match tap.mode {
                    SystemTapMode::DefaultOutput => SystemTapTarget::DefaultOutput,
                    SystemTapMode::AllOutput => SystemTapTarget::AllOutput,
                },
                tap_name: format!("mars system tap {}", tap.id),
                aggregate_uid: capture_aggregate_uid(&tap.id),
                aggregate_name: aggregate_name(&tap.id),
                mono: tap.channels == Some(1),
                private_tap: true,
                mute_behavior: TapMuteBehavior::Unmuted,
                auto_start: true,
                stream_index: 0,
            };

            let handle = match backend.create_system_tap(&request) {
                Ok(handle) => handle,
                Err(error) => {
                    cleanup_handles(&backend, &active);
                    return Err(format!("failed to create system tap '{}': {error}", tap.id));
                }
            };

            active.push(ActiveCaptureTap {
                id: tap.id.clone(),
                handle: handle.clone(),
            });
            tap_status.push(CaptureRuntimeTapStatus {
                id: tap.id.clone(),
                kind: CaptureRuntimeKind::SystemTap,
                health: CaptureRuntimeHealth::Healthy,
                selector: format!("system:{}", mode_string(tap.mode)),
                tap_id: Some(handle.tap_id),
                aggregate_uid: Some(handle.aggregate_uid.clone()),
                aggregate_device_id: Some(handle.aggregate_device_id),
                matched_processes: 0,
                last_error: None,
                ..CaptureRuntimeTapStatus::default()
            });
        }

        Ok(Self {
            backend,
            active,
            status: CaptureRuntimeStatus {
                supported: true,
                discovered_processes: processes.len(),
                active_taps: tap_status.len(),
                failed_taps: 0,
                taps: tap_status,
                errors: Vec::new(),
            },
        })
    }

    #[must_use]
    pub fn status(&self) -> CaptureRuntimeStatus {
        self.status.clone()
    }

    pub fn stop(mut self) {
        let mut failed_destroys = 0usize;
        for active in self.active.iter().rev() {
            if let Err(error) = self.backend.destroy_tap(&active.handle) {
                failed_destroys = failed_destroys.saturating_add(1);
                self.status.errors.push(format!(
                    "failed to destroy capture tap '{}' (tap_id={}, aggregate_device_id={}): {error}",
                    active.id, active.handle.tap_id, active.handle.aggregate_device_id
                ));
            }
        }

        if failed_destroys > 0 {
            self.status.failed_taps = self.status.failed_taps.saturating_add(failed_destroys);
            for tap in &mut self.status.taps {
                tap.health = CaptureRuntimeHealth::Degraded;
                if tap.last_error.is_none() {
                    tap.last_error = Some("destroy operation reported an error".to_string());
                }
            }
        }

        self.active.clear();
        self.status.active_taps = 0;
    }
}

pub fn probe_capture_capability() -> Result<TapCapability, String> {
    CoreAudioTapBackend::default().capability()
}

fn cleanup_handles(backend: &Arc<dyn TapBackend>, active: &[ActiveCaptureTap]) {
    for tap in active.iter().rev() {
        let _ = backend.destroy_tap(&tap.handle);
    }
}

fn resolve_process_selector(
    selector: &ProcessTapSelector,
    processes: &[AudioProcessInfo],
) -> Vec<AudioProcessInfo> {
    match selector {
        ProcessTapSelector::Pid { pid } => processes
            .iter()
            .filter(|process| process.pid >= 0 && process.pid as u32 == *pid)
            .cloned()
            .collect::<Vec<_>>(),
        ProcessTapSelector::BundleId { bundle_id } => {
            let mut matched = processes
                .iter()
                .filter(|process| process.bundle_id == *bundle_id)
                .cloned()
                .collect::<Vec<_>>();
            matched.sort_by_key(|process| (process.pid, process.process_object_id));
            matched
        }
    }
}

fn selector_string(selector: &ProcessTapSelector) -> String {
    match selector {
        ProcessTapSelector::Pid { pid } => format!("pid:{pid}"),
        ProcessTapSelector::BundleId { bundle_id } => format!("bundle:{bundle_id}"),
    }
}

fn mode_string(mode: SystemTapMode) -> &'static str {
    match mode {
        SystemTapMode::DefaultOutput => "default_output",
        SystemTapMode::AllOutput => "all_output",
    }
}

pub(crate) fn capture_aggregate_uid(tap_id: &str) -> String {
    format!("mars.capture.aggregate.{}.{}", std::process::id(), tap_id)
}

fn aggregate_name(tap_id: &str) -> String {
    format!("MARS Capture {}", tap_id)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use mars_audio_tap::TapHandle;
    use mars_types::{CaptureConfig, ProcessTap, ProcessTapSelector, Profile, SystemTap};

    use super::{
        AudioProcessInfo, CaptureRuntime, ProcessTapRequest, SystemTapRequest, TapBackend,
        TapCapability,
    };

    #[derive(Debug, Default)]
    struct FakeTapBackend {
        processes: Vec<AudioProcessInfo>,
        created: Mutex<Vec<String>>,
        destroyed: Mutex<Vec<String>>,
    }

    impl TapBackend for FakeTapBackend {
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
            self.created
                .lock()
                .expect("lock")
                .push(format!("process:{}", request.tap_name));
            Ok(TapHandle {
                tap_id: 10 + request.process_object_ids.len() as u32,
                tap_uid: format!("tap.process.{}", request.tap_name),
                aggregate_device_id: 100,
                aggregate_uid: request.aggregate_uid.clone(),
            })
        }

        fn create_system_tap(&self, request: &SystemTapRequest) -> Result<TapHandle, String> {
            self.created
                .lock()
                .expect("lock")
                .push(format!("system:{}", request.tap_name));
            Ok(TapHandle {
                tap_id: 20,
                tap_uid: format!("tap.system.{}", request.tap_name),
                aggregate_device_id: 200,
                aggregate_uid: request.aggregate_uid.clone(),
            })
        }

        fn destroy_tap(&self, handle: &TapHandle) -> Result<(), String> {
            self.destroyed
                .lock()
                .expect("lock")
                .push(handle.tap_uid.clone());
            Ok(())
        }
    }

    fn profile_with_captures() -> Profile {
        let mut profile = Profile::default();
        profile.captures = CaptureConfig {
            process_taps: vec![ProcessTap {
                id: "music".to_string(),
                selector: ProcessTapSelector::BundleId {
                    bundle_id: "com.example.player".to_string(),
                },
                channels: Some(2),
            }],
            system_taps: vec![SystemTap {
                id: "system".to_string(),
                mode: mars_types::SystemTapMode::AllOutput,
                channels: Some(2),
            }],
        };
        profile
    }

    #[test]
    fn runtime_start_and_stop_tracks_active_taps() {
        let backend = Arc::new(FakeTapBackend {
            processes: vec![AudioProcessInfo {
                process_object_id: 42,
                pid: 1234,
                bundle_id: "com.example.player".to_string(),
                is_running: true,
                is_running_input: false,
                is_running_output: true,
            }],
            created: Mutex::new(Vec::new()),
            destroyed: Mutex::new(Vec::new()),
        });

        let runtime = CaptureRuntime::start_with_backend(&profile_with_captures(), backend.clone())
            .expect("runtime start");
        let status = runtime.status();
        assert!(status.supported);
        assert_eq!(status.active_taps, 2);
        assert_eq!(status.failed_taps, 0);
        assert_eq!(status.discovered_processes, 1);

        runtime.stop();

        let destroyed = backend.destroyed.lock().expect("lock");
        assert_eq!(destroyed.len(), 2);
    }

    #[test]
    fn runtime_start_fails_when_selector_has_no_matches() {
        let backend = Arc::new(FakeTapBackend {
            processes: vec![AudioProcessInfo {
                process_object_id: 7,
                pid: 999,
                bundle_id: "com.other.app".to_string(),
                is_running: true,
                is_running_input: false,
                is_running_output: true,
            }],
            created: Mutex::new(Vec::new()),
            destroyed: Mutex::new(Vec::new()),
        });

        let mut profile = Profile::default();
        profile.captures.process_taps.push(ProcessTap {
            id: "target".to_string(),
            selector: ProcessTapSelector::Pid { pid: 1234 },
            channels: Some(2),
        });

        let error = CaptureRuntime::start_with_backend(&profile, backend)
            .expect_err("selector should fail");
        assert!(error.contains("did not match any active CoreAudio process object"));
    }

    #[test]
    fn bundle_selector_resolution_is_deterministic() {
        let processes = vec![
            AudioProcessInfo {
                process_object_id: 90,
                pid: 2000,
                bundle_id: "com.example.player".to_string(),
                is_running: true,
                is_running_input: false,
                is_running_output: true,
            },
            AudioProcessInfo {
                process_object_id: 11,
                pid: 1000,
                bundle_id: "com.example.player".to_string(),
                is_running: true,
                is_running_input: false,
                is_running_output: true,
            },
            AudioProcessInfo {
                process_object_id: 22,
                pid: 1000,
                bundle_id: "com.example.player".to_string(),
                is_running: true,
                is_running_input: false,
                is_running_output: true,
            },
        ];

        let matched = super::resolve_process_selector(
            &ProcessTapSelector::BundleId {
                bundle_id: "com.example.player".to_string(),
            },
            &processes,
        );
        let ordered = matched
            .iter()
            .map(|item| (item.pid, item.process_object_id))
            .collect::<Vec<_>>();
        assert_eq!(ordered, vec![(1000, 11), (1000, 22), (2000, 90)]);
    }

    #[test]
    fn process_churn_soak_start_stop_keeps_create_destroy_balanced() {
        #[derive(Debug, Default)]
        struct ChurnBackend {
            processes: Mutex<Vec<AudioProcessInfo>>,
            next_id: Mutex<u32>,
            created: Mutex<u64>,
            destroyed: Mutex<u64>,
        }

        impl TapBackend for ChurnBackend {
            fn capability(&self) -> Result<TapCapability, String> {
                Ok(TapCapability {
                    supported: true,
                    reason: None,
                })
            }

            fn list_processes(&self) -> Result<Vec<AudioProcessInfo>, String> {
                Ok(self.processes.lock().expect("lock").clone())
            }

            fn create_process_tap(&self, request: &ProcessTapRequest) -> Result<TapHandle, String> {
                let mut next = self.next_id.lock().expect("lock");
                *next += 1;
                *self.created.lock().expect("lock") += 1;
                Ok(TapHandle {
                    tap_id: *next,
                    tap_uid: format!("tap.process.{}", request.tap_name),
                    aggregate_device_id: 10_000 + *next,
                    aggregate_uid: request.aggregate_uid.clone(),
                })
            }

            fn create_system_tap(&self, request: &SystemTapRequest) -> Result<TapHandle, String> {
                let mut next = self.next_id.lock().expect("lock");
                *next += 1;
                *self.created.lock().expect("lock") += 1;
                Ok(TapHandle {
                    tap_id: *next,
                    tap_uid: format!("tap.system.{}", request.tap_name),
                    aggregate_device_id: 20_000 + *next,
                    aggregate_uid: request.aggregate_uid.clone(),
                })
            }

            fn destroy_tap(&self, _handle: &TapHandle) -> Result<(), String> {
                *self.destroyed.lock().expect("lock") += 1;
                Ok(())
            }
        }

        let backend = Arc::new(ChurnBackend {
            processes: Mutex::new(Vec::new()),
            next_id: Mutex::new(100),
            created: Mutex::new(0),
            destroyed: Mutex::new(0),
        });

        let mut profile = Profile::default();
        profile.captures.process_taps.push(ProcessTap {
            id: "app".to_string(),
            selector: ProcessTapSelector::BundleId {
                bundle_id: "com.example.player".to_string(),
            },
            channels: Some(2),
        });
        profile.captures.system_taps.push(SystemTap {
            id: "system".to_string(),
            mode: mars_types::SystemTapMode::AllOutput,
            channels: Some(2),
        });

        for iteration in 0..128_u32 {
            *backend.processes.lock().expect("lock") = vec![AudioProcessInfo {
                process_object_id: 10 + iteration,
                pid: 10_000 + iteration as i32,
                bundle_id: "com.example.player".to_string(),
                is_running: true,
                is_running_input: false,
                is_running_output: true,
            }];

            let runtime = CaptureRuntime::start_with_backend(&profile, backend.clone())
                .expect("runtime start");
            runtime.stop();
        }

        let created = *backend.created.lock().expect("lock");
        let destroyed = *backend.destroyed.lock().expect("lock");
        assert_eq!(created, destroyed);
    }
}

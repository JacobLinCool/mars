#![forbid(unsafe_code)]
//! marsd daemon implementation.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use chrono::Utc;
use mars_coreaudio::{
    CoreAudioError, ExternalEndpointConfig, ExternalIoRuntime, detect_microphone_permission,
    list_device_inventory, resolve_externals,
};
use mars_engine::{Engine, EngineSnapshot};
use mars_graph::RoutingGraph;
use mars_hal::{
    DesiredState as HalDesiredState, HalDevice as HalDeviceState, RuntimeStats as HalRuntimeStats,
};
use mars_ipc::{
    ApiError, DaemonRequest, DaemonResponse, LogRequest, LogResponse, RequestHandler, serve,
};
use mars_profile::{ValidatedProfile, load_profile, validate_only};
use mars_shm::{RingSpec, StreamDirection, global_registry, stream_name};
use mars_types::{
    ApplyPlan, ApplyRequest, ApplyResult, DaemonStatus, DeviceDescriptor, DriverStatusSummary,
    ExitCode, ExternalRuntimeStatus, MANAGED_UID_PREFIX, NodeKind, PlanChange, PlanChangeKind,
    PlanRequest, Profile, RuntimeCounters,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

#[derive(Debug)]
pub struct MarsDaemon {
    state: Mutex<DaemonState>,
    render_runtime: Mutex<Option<RenderRuntime>>,
    log_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct DaemonState {
    current_profile_path: Option<String>,
    current_profile: Option<Profile>,
    graph: Option<RoutingGraph>,
    engine: Option<Arc<Engine>>,
    devices: Vec<DeviceDescriptor>,
    counters: RuntimeCounters,
    external_runtime: ExternalRuntimeStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DriverAppliedState {
    version: String,
    devices: Vec<DriverDevice>,
    sample_rate: u32,
    channels: u16,
    #[serde(default = "default_driver_buffer_frames")]
    buffer_frames: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DriverDevice {
    id: String,
    uid: String,
    name: String,
    kind: NodeKind,
    channels: u16,
    #[serde(default)]
    hidden: bool,
}

#[derive(Debug, Clone)]
struct DriverStateSnapshot {
    path: PathBuf,
    raw_json: Option<String>,
    parsed_state: Option<DriverAppliedState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderEndpoint {
    node_id: String,
    uid: String,
    channels: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderRuntimeConfig {
    sample_rate: u32,
    buffer_frames: u32,
    vout_sources: Vec<RenderEndpoint>,
    vin_sinks: Vec<RenderEndpoint>,
    external_inputs: Vec<RenderEndpoint>,
    external_outputs: Vec<RenderEndpoint>,
}

#[derive(Debug, Default)]
struct RenderMetrics {
    deadline_miss_count: AtomicU64,
    last_cycle_ns: AtomicU64,
    max_cycle_ns: AtomicU64,
    external_runtime: Mutex<ExternalRuntimeStatus>,
    external_underrun_count: AtomicU64,
    external_overrun_count: AtomicU64,
    external_xrun_count: AtomicU64,
}

#[derive(Debug)]
struct RenderRuntime {
    config: RenderRuntimeConfig,
    external_runtime: Option<Arc<ExternalIoRuntime>>,
    stop: Arc<AtomicBool>,
    metrics: Arc<RenderMetrics>,
    handle: Option<JoinHandle<()>>,
}

const fn default_driver_buffer_frames() -> u32 {
    256
}

impl RenderRuntime {
    fn start(config: RenderRuntimeConfig, engine: Arc<Engine>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(RenderMetrics::default());
        let external_runtime = if config.external_inputs.is_empty()
            && config.external_outputs.is_empty()
        {
            None
        } else {
            let input_configs = config
                .external_inputs
                .iter()
                .map(|endpoint| ExternalEndpointConfig {
                    node_id: endpoint.node_id.clone(),
                    uid: endpoint.uid.clone(),
                    channels: endpoint.channels,
                })
                .collect::<Vec<_>>();
            let output_configs = config
                .external_outputs
                .iter()
                .map(|endpoint| ExternalEndpointConfig {
                    node_id: endpoint.node_id.clone(),
                    uid: endpoint.uid.clone(),
                    channels: endpoint.channels,
                })
                .collect::<Vec<_>>();
            let runtime = ExternalIoRuntime::start(
                config.sample_rate,
                config.buffer_frames,
                &input_configs,
                &output_configs,
            )
            .map_err(|error| format!("external I/O runtime startup failed: {error}"))?;
            let snapshot = runtime.snapshot();
            if snapshot.status.connected_inputs != input_configs.len()
                || snapshot.status.connected_outputs != output_configs.len()
            {
                return Err(format!(
                    "external I/O runtime readiness failed: expected inputs={} outputs={}, got inputs={} outputs={}",
                    input_configs.len(),
                    output_configs.len(),
                    snapshot.status.connected_inputs,
                    snapshot.status.connected_outputs
                ));
            }
            Some(Arc::new(runtime))
        };

        if let Some(external_runtime) = external_runtime.as_ref() {
            let snapshot = external_runtime.snapshot();
            *metrics.external_runtime.lock() = snapshot.status.clone();
            metrics
                .external_underrun_count
                .store(snapshot.counters.underrun_count, Ordering::Relaxed);
            metrics
                .external_overrun_count
                .store(snapshot.counters.overrun_count, Ordering::Relaxed);
            metrics
                .external_xrun_count
                .store(snapshot.counters.xrun_count, Ordering::Relaxed);
        }

        let thread_stop = stop.clone();
        let thread_metrics = metrics.clone();
        let thread_config = config.clone();
        let thread_external_runtime = external_runtime.clone();
        let handle = std::thread::Builder::new()
            .name("marsd-render".to_string())
            .spawn(move || {
                run_render_loop(
                    engine,
                    thread_config,
                    thread_stop,
                    thread_metrics,
                    thread_external_runtime,
                );
            })
            .map_err(|error| format!("failed to spawn render runtime thread: {error}"))?;

        Ok(Self {
            config,
            external_runtime,
            stop,
            metrics,
            handle: Some(handle),
        })
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn update_max_atomic(cell: &AtomicU64, value: u64) {
    let mut current = cell.load(Ordering::Relaxed);
    while value > current {
        match cell.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}

fn run_render_loop(
    engine: Arc<Engine>,
    config: RenderRuntimeConfig,
    stop: Arc<AtomicBool>,
    metrics: Arc<RenderMetrics>,
    external_runtime: Option<Arc<ExternalIoRuntime>>,
) {
    let frames = config.buffer_frames as usize;
    let period = Duration::from_secs_f64(config.buffer_frames as f64 / config.sample_rate as f64);
    let inject_sleep_ms = std::env::var("MARS_RENDER_LOOP_INJECT_SLEEP_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    let mut source_buffers = HashMap::<String, Vec<f32>>::new();
    for source in &config.vout_sources {
        source_buffers.insert(
            source.node_id.clone(),
            vec![0.0; frames.saturating_mul(source.channels as usize)],
        );
    }
    for source in &config.external_inputs {
        source_buffers.insert(
            source.node_id.clone(),
            vec![0.0; frames.saturating_mul(source.channels as usize)],
        );
    }

    let mut sink_silence = HashMap::<String, Vec<f32>>::new();
    for sink in &config.vin_sinks {
        sink_silence.insert(
            sink.node_id.clone(),
            vec![0.0; frames.saturating_mul(sink.channels as usize)],
        );
    }
    for sink in &config.external_outputs {
        sink_silence.insert(
            sink.node_id.clone(),
            vec![0.0; frames.saturating_mul(sink.channels as usize)],
        );
    }

    let mut source_rings = config
        .vout_sources
        .iter()
        .map(|endpoint| (endpoint.clone(), None))
        .collect::<Vec<(RenderEndpoint, Option<mars_shm::SharedRingHandle>)>>();
    let mut sink_rings = config
        .vin_sinks
        .iter()
        .map(|endpoint| (endpoint.clone(), None))
        .collect::<Vec<(RenderEndpoint, Option<mars_shm::SharedRingHandle>)>>();

    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();

        for (endpoint, ring_handle) in &mut source_rings {
            if ring_handle.is_none() {
                let spec = RingSpec {
                    sample_rate: config.sample_rate,
                    channels: endpoint.channels,
                    capacity_frames: config.buffer_frames.saturating_mul(8),
                };
                let name = stream_name(StreamDirection::Vout, &endpoint.uid);
                *ring_handle = global_registry().create_or_open(&name, spec).ok();
            }

            let Some(samples) = source_buffers.get_mut(&endpoint.node_id) else {
                continue;
            };
            if let Some(handle) = ring_handle {
                let mut guard = handle.lock();
                match guard.read_interleaved(samples) {
                    Ok(read_frames) => {
                        let keep = read_frames.saturating_mul(endpoint.channels as usize);
                        if keep < samples.len() {
                            samples[keep..].fill(0.0);
                        }
                    }
                    Err(_) => samples.fill(0.0),
                }
            } else {
                samples.fill(0.0);
            }
        }

        if let Some(external_runtime) = external_runtime.as_ref() {
            for endpoint in &config.external_inputs {
                let Some(samples) = source_buffers.get_mut(&endpoint.node_id) else {
                    continue;
                };
                if !external_runtime.read_input_into(&endpoint.node_id, samples) {
                    samples.fill(0.0);
                }
            }
        } else {
            for endpoint in &config.external_inputs {
                if let Some(samples) = source_buffers.get_mut(&endpoint.node_id) {
                    samples.fill(0.0);
                }
            }
        }

        if let Ok(rendered) = engine.render_cycle(frames, &source_buffers) {
            for (endpoint, ring_handle) in &mut sink_rings {
                if ring_handle.is_none() {
                    let spec = RingSpec {
                        sample_rate: config.sample_rate,
                        channels: endpoint.channels,
                        capacity_frames: config.buffer_frames.saturating_mul(8),
                    };
                    let name = stream_name(StreamDirection::Vin, &endpoint.uid);
                    *ring_handle = global_registry().create_or_open(&name, spec).ok();
                }

                let Some(handle) = ring_handle else {
                    continue;
                };
                let data = rendered.sinks.get(&endpoint.node_id).map_or_else(
                    || {
                        sink_silence
                            .get(&endpoint.node_id)
                            .map(Vec::as_slice)
                            .unwrap_or(&[])
                    },
                    Vec::as_slice,
                );
                let mut guard = handle.lock();
                let _ = guard.write_interleaved(data);
            }

            if let Some(external_runtime) = external_runtime.as_ref() {
                for endpoint in &config.external_outputs {
                    let data = rendered.sinks.get(&endpoint.node_id).map_or_else(
                        || {
                            sink_silence
                                .get(&endpoint.node_id)
                                .map(Vec::as_slice)
                                .unwrap_or(&[])
                        },
                        Vec::as_slice,
                    );
                    let _ = external_runtime.write_output_from(&endpoint.node_id, data);
                }
            }
        }

        if let Some(external_runtime) = external_runtime.as_ref() {
            let snapshot = external_runtime.snapshot();
            *metrics.external_runtime.lock() = snapshot.status.clone();
            metrics
                .external_underrun_count
                .store(snapshot.counters.underrun_count, Ordering::Relaxed);
            metrics
                .external_overrun_count
                .store(snapshot.counters.overrun_count, Ordering::Relaxed);
            metrics
                .external_xrun_count
                .store(snapshot.counters.xrun_count, Ordering::Relaxed);
        }

        if inject_sleep_ms > 0 {
            std::thread::sleep(Duration::from_millis(inject_sleep_ms));
        }

        let cycle_ns = started.elapsed().as_nanos() as u64;
        metrics.last_cycle_ns.store(cycle_ns, Ordering::Relaxed);
        update_max_atomic(&metrics.max_cycle_ns, cycle_ns);
        if cycle_ns > period.as_nanos() as u64 {
            metrics.deadline_miss_count.fetch_add(1, Ordering::Relaxed);
        } else {
            let remain = period.saturating_sub(Duration::from_nanos(cycle_ns));
            if !remain.is_zero() {
                std::thread::sleep(remain);
            }
        }
    }
}

impl MarsDaemon {
    #[must_use]
    pub fn new(log_path: PathBuf) -> Self {
        Self {
            state: Mutex::new(DaemonState::default()),
            render_runtime: Mutex::new(None),
            log_path,
        }
    }

    pub async fn run(self: Arc<Self>, socket_path: &Path) -> Result<(), mars_ipc::IpcError> {
        serve(socket_path, self).await
    }

    fn stop_render_runtime(&self) {
        if let Some(runtime) = self.render_runtime.lock().take() {
            runtime.stop();
        }
    }

    fn sync_render_runtime(&self) -> Result<(), String> {
        let (engine, config) = {
            let state = self.state.lock();
            (
                state.engine.clone(),
                render_runtime_config_from_state(state.current_profile.as_ref(), &state.devices),
            )
        };

        let mut runtime = self.render_runtime.lock();
        match (engine, config) {
            (Some(engine), Some(config)) => {
                let needs_restart = match runtime.as_ref() {
                    None => true,
                    Some(existing) => existing.config != config,
                };
                if needs_restart {
                    if let Some(existing) = runtime.take() {
                        existing.stop();
                    }
                    *runtime = Some(RenderRuntime::start(config, engine)?);
                }
            }
            _ => {
                if let Some(existing) = runtime.take() {
                    existing.stop();
                }
            }
        }

        Ok(())
    }

    fn apply_deadline(timeout_ms: u64) -> Option<Instant> {
        if timeout_ms == 0 {
            None
        } else {
            Instant::now().checked_add(Duration::from_millis(timeout_ms))
        }
    }

    fn ensure_within_deadline(deadline: Option<Instant>, stage: &str) -> Result<(), ApiError> {
        if let Some(deadline) = deadline {
            if Instant::now() > deadline {
                return Err(ApiError::new(
                    format!("apply timeout exceeded during stage: {stage}"),
                    ExitCode::ApplyFailed,
                ));
            }
        }
        Ok(())
    }

    fn rollback_apply(
        &self,
        previous_state: DaemonState,
        driver_snapshot: &DriverStateSnapshot,
        stage_error: impl Into<String>,
        exit_code: ExitCode,
    ) -> ApiError {
        self.stop_render_runtime();

        let mut rollback_issues = Vec::new();
        if let Err(error) = restore_driver_state_snapshot(driver_snapshot) {
            rollback_issues.push(format!("driver rollback failed: {error}"));
        }

        {
            let mut state = self.state.lock();
            *state = previous_state;
        }

        if let Err(error) = self.sync_render_runtime() {
            rollback_issues.push(format!("render runtime rollback failed: {error}"));
        }

        let stage_error = stage_error.into();
        if rollback_issues.is_empty() {
            ApiError::new(
                format!("apply failed and rolled back: {stage_error}"),
                exit_code,
            )
        } else {
            ApiError::new(
                format!(
                    "apply failed and rollback encountered issues: {stage_error}; {}",
                    rollback_issues.join("; ")
                ),
                exit_code,
            )
        }
    }

    fn plan_internal(
        &self,
        request: &PlanRequest,
        validated: &ValidatedProfile,
    ) -> Result<ApplyPlan, ApiError> {
        let mut warnings = validated.warnings.clone();
        let inventory = list_device_inventory().map_err(coreaudio_to_api_error)?;
        let resolution = resolve_externals(&validated.profile, &inventory);

        if !resolution.errors.is_empty() {
            return Err(ApiError::new(
                format!("missing external devices: {}", resolution.errors.join("; ")),
                ExitCode::MissingExternal,
            ));
        }

        warnings.extend(resolution.warnings);

        let state = self.state.lock();
        let previous = state.current_profile.as_ref();
        let mut plan = diff_profiles(previous, Some(&validated.profile));

        if request.no_delete {
            plan.changes
                .retain(|change| change.kind != PlanChangeKind::RemoveDevice);
        }

        if plan.changes.is_empty() {
            plan.changes.push(PlanChange {
                kind: PlanChangeKind::NoOp,
                target: "state".to_string(),
                details: "already converged".to_string(),
            });
        }
        plan.warnings.extend(warnings);

        Ok(plan)
    }

    fn apply_internal(&self, request: ApplyRequest) -> Result<ApplyResult, ApiError> {
        let deadline = Self::apply_deadline(request.timeout_ms);
        Self::ensure_within_deadline(deadline, "profile-validate")?;

        let validated = load_profile(Path::new(&request.profile_path)).map_err(|error| {
            ApiError::new(
                format!(
                    "profile validation failed (strict policy requires apply_mode=atomic, on_missing_external=error, and no endpoint fallback/on_missing override): {error}"
                ),
                ExitCode::InvalidInput,
            )
        })?;

        let plan_request = PlanRequest {
            profile_path: request.profile_path.clone(),
            no_delete: request.no_delete,
        };

        Self::ensure_within_deadline(deadline, "plan")?;
        let plan = self.plan_internal(&plan_request, &validated)?;
        if request.dry_run {
            return Ok(ApplyResult {
                applied: false,
                plan,
                warnings: Vec::new(),
                errors: Vec::new(),
            });
        }

        Self::ensure_within_deadline(deadline, "external-resolve")?;
        let inventory = list_device_inventory().map_err(coreaudio_to_api_error)?;
        let resolution = resolve_externals(&validated.profile, &inventory);
        if !resolution.errors.is_empty() {
            return Err(ApiError::new(
                format!("missing external devices: {}", resolution.errors.join("; ")),
                ExitCode::MissingExternal,
            ));
        }

        let previous_state = self.state.lock().clone();
        let driver_snapshot_path = driver_state_path()
            .map_err(|error| ApiError::new(error, ExitCode::DriverUnavailable))?;
        let driver_snapshot = capture_driver_state_snapshot(&driver_snapshot_path)
            .map_err(|error| ApiError::new(error, ExitCode::DriverUnavailable))?;

        // When --no-delete is set, merge devices from the previous profile that
        // would otherwise be removed so the HAL desired state retains them.
        let effective_profile = if request.no_delete {
            if let Some(prev) = previous_state.current_profile.clone() {
                let next_output_ids: BTreeSet<&str> = validated
                    .profile
                    .virtual_devices
                    .outputs
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect();
                let next_input_ids: BTreeSet<&str> = validated
                    .profile
                    .virtual_devices
                    .inputs
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect();

                let mut merged = validated.profile.clone();
                for output in &prev.virtual_devices.outputs {
                    if !next_output_ids.contains(output.id.as_str()) {
                        merged.virtual_devices.outputs.push(output.clone());
                    }
                }
                for input in &prev.virtual_devices.inputs {
                    if !next_input_ids.contains(input.id.as_str()) {
                        merged.virtual_devices.inputs.push(input.clone());
                    }
                }
                merged
            } else {
                validated.profile.clone()
            }
        } else {
            validated.profile.clone()
        };

        let sample_rate = effective_profile
            .audio
            .sample_rate
            .as_value()
            .unwrap_or(48_000);
        let channels = effective_profile.audio.channels.as_value().unwrap_or(2);
        let warnings = plan.warnings.clone();
        Self::ensure_within_deadline(deadline, "driver-compatibility")?;
        if let Err(error) = ensure_driver_compatibility_for_apply() {
            return Err(ApiError::new(error, ExitCode::DriverUnavailable));
        }

        let devices = collect_devices(&effective_profile, &resolution.resolved);

        self.stop_render_runtime();

        Self::ensure_within_deadline(deadline, "driver-stage").map_err(|timeout_error| {
            self.rollback_apply(
                previous_state.clone(),
                &driver_snapshot,
                timeout_error.message,
                ExitCode::ApplyFailed,
            )
        })?;
        if let Err(error) =
            stage_driver_state(&effective_profile, &validated.graph, sample_rate, channels)
        {
            warn!(error = %error, "failed to stage driver state");
            return Err(self.rollback_apply(
                previous_state,
                &driver_snapshot,
                error,
                ExitCode::DriverUnavailable,
            ));
        }
        Self::ensure_within_deadline(deadline, "graph-activate").map_err(|timeout_error| {
            self.rollback_apply(
                previous_state.clone(),
                &driver_snapshot,
                timeout_error.message,
                ExitCode::ApplyFailed,
            )
        })?;

        let mut state = self.state.lock();
        let engine = if let (Some(previous_profile), Some(existing_engine)) =
            (state.current_profile.as_ref(), state.engine.as_ref())
        {
            if render_restart_required(previous_profile, &effective_profile) {
                Arc::new(Engine::new(EngineSnapshot {
                    graph: validated.graph.clone(),
                    sample_rate,
                    buffer_frames: effective_profile.audio.buffer_frames,
                }))
            } else {
                existing_engine.swap_snapshot(EngineSnapshot {
                    graph: validated.graph.clone(),
                    sample_rate,
                    buffer_frames: effective_profile.audio.buffer_frames,
                });
                existing_engine.clone()
            }
        } else {
            Arc::new(Engine::new(EngineSnapshot {
                graph: validated.graph.clone(),
                sample_rate,
                buffer_frames: effective_profile.audio.buffer_frames,
            }))
        };

        state.current_profile_path = Some(request.profile_path);
        state.current_profile = Some(effective_profile);
        state.graph = Some(validated.graph.clone());
        state.engine = Some(engine);
        state.devices = devices;
        state.external_runtime = ExternalRuntimeStatus::default();
        drop(state);

        if let Err(error) = self.sync_render_runtime() {
            return Err(self.rollback_apply(
                previous_state.clone(),
                &driver_snapshot,
                error,
                ExitCode::ApplyFailed,
            ));
        }

        Self::ensure_within_deadline(deadline, "runtime-ready").map_err(|timeout_error| {
            self.rollback_apply(
                previous_state.clone(),
                &driver_snapshot,
                timeout_error.message,
                ExitCode::ApplyFailed,
            )
        })?;

        {
            let runtime_guard = self.render_runtime.lock();
            if let Some(runtime) = runtime_guard.as_ref() {
                if let Some(external_runtime) = runtime.external_runtime.as_ref() {
                    let snapshot = external_runtime.snapshot();
                    let expected_inputs = runtime.config.external_inputs.len();
                    let expected_outputs = runtime.config.external_outputs.len();
                    if snapshot.status.connected_inputs != expected_inputs
                        || snapshot.status.connected_outputs != expected_outputs
                    {
                        return Err(self.rollback_apply(
                            previous_state.clone(),
                            &driver_snapshot,
                            format!(
                                "external runtime readiness failed: expected inputs={} outputs={}, got inputs={} outputs={}",
                                expected_inputs,
                                expected_outputs,
                                snapshot.status.connected_inputs,
                                snapshot.status.connected_outputs
                            ),
                            ExitCode::ApplyFailed,
                        ));
                    }
                }
            }
        }

        if let Some(external_status) = self
            .render_runtime
            .lock()
            .as_ref()
            .map(|runtime| runtime.metrics.external_runtime.lock().clone())
        {
            self.state.lock().external_runtime = external_status;
        }

        info!("apply transaction committed");

        Ok(ApplyResult {
            applied: true,
            plan,
            warnings,
            errors: Vec::new(),
        })
    }

    fn clear_internal(&self, keep_devices: bool) -> Result<ApplyResult, ApiError> {
        self.stop_render_runtime();
        let mut state = self.state.lock();
        let previous = state.clone();

        state.engine = None;
        state.current_profile = None;
        state.current_profile_path = None;
        state.graph = None;
        state.external_runtime = ExternalRuntimeStatus::default();

        if !keep_devices {
            state.devices.retain(|device| !device.managed);
            if let Err(error) = clear_driver_state() {
                *state = previous;
                drop(state);
                if let Err(error) = self.sync_render_runtime() {
                    warn!(error = %error, "failed to restore render runtime during clear rollback");
                }
                return Err(ApiError::new(
                    format!("clear failed and rolled back: {error}"),
                    ExitCode::DriverUnavailable,
                ));
            }
        }
        drop(state);

        Ok(ApplyResult {
            applied: true,
            plan: ApplyPlan {
                changes: vec![PlanChange {
                    kind: PlanChangeKind::UpdateGraph,
                    target: "state".to_string(),
                    details: if keep_devices {
                        "engine stopped, devices kept".to_string()
                    } else {
                        "engine stopped, managed devices removed".to_string()
                    },
                }],
                warnings: Vec::new(),
            },
            warnings: Vec::new(),
            errors: Vec::new(),
        })
    }

    fn status_internal(&self) -> DaemonStatus {
        let (
            running,
            current_profile,
            sample_rate,
            buffer_frames,
            graph_pipe_count,
            devices,
            mut counters,
            mut external_runtime,
        ) = {
            let state = self.state.lock();
            let profile = state.current_profile.as_ref();
            (
                state.engine.is_some(),
                state.current_profile_path.clone(),
                profile
                    .and_then(|profile| profile.audio.sample_rate.as_value())
                    .unwrap_or(48_000),
                profile.map_or(256, |profile| profile.audio.buffer_frames),
                state.graph.as_ref().map_or(0, |graph| graph.edges.len()),
                state.devices.clone(),
                state.counters.clone(),
                state.external_runtime.clone(),
            )
        };

        let driver = driver_status_summary();
        if let Some(runtime) = driver_runtime_stats() {
            counters.underrun_count = runtime.underrun_count;
            counters.overrun_count = runtime.overrun_count;
            counters.xrun_count = runtime.xrun_count;
            counters.last_callback_ns = runtime.last_callback_ns;
        }
        if let Some(render_runtime) = self.render_runtime.lock().as_ref() {
            counters.deadline_miss_count = render_runtime
                .metrics
                .deadline_miss_count
                .load(Ordering::Relaxed);
            counters.last_cycle_ns = render_runtime.metrics.last_cycle_ns.load(Ordering::Relaxed);
            counters.max_cycle_ns = render_runtime.metrics.max_cycle_ns.load(Ordering::Relaxed);
            counters.underrun_count = counters.underrun_count.saturating_add(
                render_runtime
                    .metrics
                    .external_underrun_count
                    .load(Ordering::Relaxed),
            );
            counters.overrun_count = counters.overrun_count.saturating_add(
                render_runtime
                    .metrics
                    .external_overrun_count
                    .load(Ordering::Relaxed),
            );
            counters.xrun_count = counters.xrun_count.saturating_add(
                render_runtime
                    .metrics
                    .external_xrun_count
                    .load(Ordering::Relaxed),
            );
            external_runtime = render_runtime.metrics.external_runtime.lock().clone();
        }

        DaemonStatus {
            running,
            current_profile,
            sample_rate,
            buffer_frames,
            graph_pipe_count,
            devices,
            counters,
            driver,
            external_runtime,
            updated_at: Utc::now(),
        }
    }

    fn logs_internal(&self, request: &LogRequest) -> Result<LogResponse, ApiError> {
        let data = fs::read_to_string(&self.log_path).map_err(|error| {
            ApiError::new(
                format!(
                    "failed to read log file {}: {error}",
                    self.log_path.display()
                ),
                ExitCode::InvalidInput,
            )
        })?;
        let all_lines = data.lines().map(ToOwned::to_owned).collect::<VecDeque<_>>();
        let total = all_lines.len() as u64;
        let limit = request.limit.unwrap_or(200) as usize;

        let start = if let Some(cursor) = request.cursor {
            if cursor <= total {
                cursor as usize
            } else {
                total.saturating_sub(limit as u64) as usize
            }
        } else {
            total.saturating_sub(limit as u64) as usize
        };

        let lines = all_lines.into_iter().skip(start).collect::<Vec<_>>();

        Ok(LogResponse {
            lines,
            next_cursor: total,
        })
    }

    fn doctor_report_internal(&self) -> mars_types::DoctorReport {
        let driver_installed = Path::new("/Library/Audio/Plug-Ins/HAL/mars.driver").exists();
        let daemon_reachable = true;
        let driver_loaded = if driver_installed {
            mars_hal_client::is_driver_loaded()
        } else {
            false
        };
        let driver = driver_status_summary();
        let daemon_version = env!("CARGO_PKG_VERSION").to_string();
        let driver_version = read_driver_version();
        let driver_compatible = is_driver_compatible(
            driver_installed,
            driver_loaded,
            driver_version.as_deref(),
            &daemon_version,
        );

        let env_assume_mic = std::env::var("MARS_ASSUME_MIC_PERMISSION")
            .as_deref()
            .map(|value| value == "1")
            .unwrap_or(false);
        let (microphone_permission_ok, mic_permission_source) = if env_assume_mic {
            (true, "env_override".to_string())
        } else if let Some(status) = detect_microphone_permission() {
            (status, "tcc".to_string())
        } else {
            (false, "unknown".to_string())
        };

        let mut notes = Vec::new();
        if !driver_installed {
            notes.push("driver not found at /Library/Audio/Plug-Ins/HAL/mars.driver".to_string());
        }
        if driver_installed && !driver_loaded {
            notes.push("mars.driver is installed but not loaded by coreaudiod; run: sudo killall -9 coreaudiod".to_string());
        }
        if !microphone_permission_ok {
            notes.push(
                "microphone permission is not granted or could not be verified; check System Settings > Privacy & Security > Microphone".to_string(),
            );
        }
        if !driver_compatible {
            notes.push(format!(
                "driver/daemon version mismatch: driver={:?}, daemon={}",
                driver_version, daemon_version
            ));
        }
        if driver.pending_change {
            notes.push("driver has a pending configuration change".to_string());
        }

        let state = self.state.lock();
        notes.extend(feedback_risk_notes(state.graph.as_ref(), &state.devices));
        drop(state);

        mars_types::DoctorReport {
            driver_installed,
            driver_compatible,
            daemon_reachable,
            microphone_permission_ok,
            driver_version,
            daemon_version,
            mic_permission_source,
            driver,
            notes,
        }
    }
}

#[async_trait]
impl RequestHandler for MarsDaemon {
    async fn handle(&self, request: DaemonRequest) -> Result<DaemonResponse, ApiError> {
        match request {
            DaemonRequest::Ping => Ok(DaemonResponse::Pong),
            DaemonRequest::Validate(request) => {
                let report = validate_only(Path::new(&request.profile_path));
                Ok(DaemonResponse::Validate(report))
            }
            DaemonRequest::Plan(request) => {
                let validated =
                    load_profile(Path::new(&request.profile_path)).map_err(|error| {
                        ApiError::new(
                            format!(
                                "profile validation failed (strict policy requires apply_mode=atomic, on_missing_external=error, and no endpoint fallback/on_missing override): {error}"
                            ),
                            ExitCode::InvalidInput,
                        )
                    })?;
                let plan = self.plan_internal(&request, &validated)?;
                Ok(DaemonResponse::Plan(plan))
            }
            DaemonRequest::Apply(request) => {
                self.apply_internal(request).map(DaemonResponse::Apply)
            }
            DaemonRequest::Clear(request) => self
                .clear_internal(request.keep_devices)
                .map(DaemonResponse::Clear),
            DaemonRequest::Status => Ok(DaemonResponse::Status(self.status_internal())),
            DaemonRequest::Devices => {
                let inventory = list_device_inventory().map_err(coreaudio_to_api_error)?;
                Ok(DaemonResponse::Devices(inventory))
            }
            DaemonRequest::Logs(request) => self.logs_internal(&request).map(DaemonResponse::Logs),
            DaemonRequest::Doctor => Ok(DaemonResponse::Doctor(self.doctor_report_internal())),
        }
    }
}

fn coreaudio_to_api_error(error: CoreAudioError) -> ApiError {
    ApiError::new(
        format!("coreaudio operation failed: {error}"),
        ExitCode::ApplyFailed,
    )
}

fn node_kind_to_hal_kind(kind: NodeKind) -> String {
    match kind {
        NodeKind::VirtualOutput => "virtual_output".to_string(),
        NodeKind::VirtualInput => "virtual_input".to_string(),
        NodeKind::ExternalInput => "external_input".to_string(),
        NodeKind::ExternalOutput => "external_output".to_string(),
        NodeKind::Bus => "bus".to_string(),
    }
}

fn driver_status_summary() -> DriverStatusSummary {
    match mars_hal_client::get_configuration_summary() {
        Ok(summary) => DriverStatusSummary {
            generation: summary.current_generation,
            request_count: summary.request_count,
            perform_count: summary.perform_count,
            applied_device_count: summary.applied_device_count,
            pending_change: summary.pending.is_some(),
        },
        Err(error) => {
            warn!(error = %error, "failed to query driver configuration summary");
            DriverStatusSummary::default()
        }
    }
}

fn driver_runtime_stats() -> Option<HalRuntimeStats> {
    match mars_hal_client::get_runtime_stats() {
        Ok(stats) => Some(HalRuntimeStats {
            underrun_count: stats.underrun_count,
            overrun_count: stats.overrun_count,
            xrun_count: stats.xrun_count,
            last_callback_ns: stats.last_callback_ns,
        }),
        Err(error) => {
            warn!(error = %error, "failed to query driver runtime stats");
            None
        }
    }
}

fn diff_profiles(previous: Option<&Profile>, next: Option<&Profile>) -> ApplyPlan {
    let mut changes = Vec::new();

    let previous_virtual = previous.map(virtual_device_map).unwrap_or_default();
    let next_virtual = next.map(virtual_device_map).unwrap_or_default();

    for (id, next_device) in &next_virtual {
        match previous_virtual.get(id) {
            None => changes.push(PlanChange {
                kind: PlanChangeKind::CreateDevice,
                target: id.clone(),
                details: format!("create {} ({})", next_device.name, next_device.uid),
            }),
            Some(prev_device)
                if prev_device.name != next_device.name
                    || prev_device.channels != next_device.channels
                    || prev_device.uid != next_device.uid =>
            {
                changes.push(PlanChange {
                    kind: PlanChangeKind::UpdateDevice,
                    target: id.clone(),
                    details: format!("update {} ({})", next_device.name, next_device.uid),
                });
            }
            _ => {}
        }
    }

    for (id, prev_device) in &previous_virtual {
        if !next_virtual.contains_key(id) {
            changes.push(PlanChange {
                kind: PlanChangeKind::RemoveDevice,
                target: id.clone(),
                details: format!("remove {} ({})", prev_device.name, prev_device.uid),
            });
        }
    }

    if let (Some(previous), Some(next)) = (previous, next) {
        if previous.audio != next.audio {
            changes.push(PlanChange {
                kind: PlanChangeKind::UpdateAudioConfig,
                target: "audio".to_string(),
                details: "update sample rate/channels/buffer".to_string(),
            });
        }

        if previous.pipes != next.pipes || previous.buses != next.buses {
            changes.push(PlanChange {
                kind: PlanChangeKind::UpdateGraph,
                target: "graph".to_string(),
                details: "update routing graph".to_string(),
            });
        }
    }

    if previous.is_none() && next.is_some() {
        changes.push(PlanChange {
            kind: PlanChangeKind::UpdateGraph,
            target: "graph".to_string(),
            details: "initialize routing graph".to_string(),
        });
    }

    if previous.is_some() && next.is_none() {
        changes.push(PlanChange {
            kind: PlanChangeKind::UpdateGraph,
            target: "graph".to_string(),
            details: "clear routing graph".to_string(),
        });
    }

    ApplyPlan {
        changes,
        warnings: Vec::new(),
    }
}

#[derive(Debug)]
struct VirtualDeviceDiffEntry {
    name: String,
    uid: String,
    channels: u16,
}

fn virtual_device_map(profile: &Profile) -> BTreeMap<String, VirtualDeviceDiffEntry> {
    let default_channels = profile.audio.channels.as_value().unwrap_or(2);
    let mut map = BTreeMap::new();

    for output in &profile.virtual_devices.outputs {
        map.insert(
            output.id.clone(),
            VirtualDeviceDiffEntry {
                name: output.name.clone(),
                uid: output
                    .uid
                    .clone()
                    .unwrap_or_else(|| format!("{MANAGED_UID_PREFIX}vout.{}", output.id)),
                channels: output.channels.unwrap_or(default_channels),
            },
        );
    }

    for input in &profile.virtual_devices.inputs {
        map.insert(
            input.id.clone(),
            VirtualDeviceDiffEntry {
                name: input.name.clone(),
                uid: input
                    .uid
                    .clone()
                    .unwrap_or_else(|| format!("{MANAGED_UID_PREFIX}vin.{}", input.id)),
                channels: input.channels.unwrap_or(default_channels),
            },
        );
    }

    map
}

fn collect_devices(
    profile: &Profile,
    resolved_external: &[mars_types::ResolvedExternalDevice],
) -> Vec<DeviceDescriptor> {
    let default_channels = profile.audio.channels.as_value().unwrap_or(2);
    let mut devices = Vec::new();

    for output in &profile.virtual_devices.outputs {
        let uid = output
            .uid
            .clone()
            .unwrap_or_else(|| format!("{MANAGED_UID_PREFIX}vout.{}", output.id));
        devices.push(DeviceDescriptor {
            id: output.id.clone(),
            name: output.name.clone(),
            uid,
            kind: NodeKind::VirtualOutput,
            channels: output.channels.unwrap_or(default_channels),
            managed: true,
        });
    }

    for input in &profile.virtual_devices.inputs {
        let uid = input
            .uid
            .clone()
            .unwrap_or_else(|| format!("{MANAGED_UID_PREFIX}vin.{}", input.id));
        devices.push(DeviceDescriptor {
            id: input.id.clone(),
            name: input.name.clone(),
            uid,
            kind: NodeKind::VirtualInput,
            channels: input.channels.unwrap_or(default_channels),
            managed: true,
        });
    }

    for resolved in resolved_external {
        devices.push(DeviceDescriptor {
            id: resolved.logical_id.clone(),
            name: resolved.name.clone(),
            uid: resolved.matched_uid.clone(),
            kind: resolved.kind,
            channels: resolved.channels,
            managed: false,
        });
    }

    dedupe_devices(devices)
}

fn dedupe_devices(devices: Vec<DeviceDescriptor>) -> Vec<DeviceDescriptor> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();

    for device in devices {
        if seen.insert(device.id.clone()) {
            out.push(device);
        }
    }

    out
}

fn render_runtime_config_from_state(
    profile: Option<&Profile>,
    devices: &[DeviceDescriptor],
) -> Option<RenderRuntimeConfig> {
    let profile = profile?;
    let sample_rate = profile.audio.sample_rate.as_value().unwrap_or(48_000);
    let buffer_frames = profile.audio.buffer_frames;

    let mut vout_sources = devices
        .iter()
        .filter(|device| matches!(device.kind, NodeKind::VirtualOutput))
        .map(|device| RenderEndpoint {
            node_id: device.id.clone(),
            uid: device.uid.clone(),
            channels: device.channels,
        })
        .collect::<Vec<_>>();
    vout_sources.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    let mut vin_sinks = devices
        .iter()
        .filter(|device| matches!(device.kind, NodeKind::VirtualInput))
        .map(|device| RenderEndpoint {
            node_id: device.id.clone(),
            uid: device.uid.clone(),
            channels: device.channels,
        })
        .collect::<Vec<_>>();
    vin_sinks.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    let mut external_inputs = devices
        .iter()
        .filter(|device| matches!(device.kind, NodeKind::ExternalInput))
        .map(|device| RenderEndpoint {
            node_id: device.id.clone(),
            uid: device.uid.clone(),
            channels: device.channels,
        })
        .collect::<Vec<_>>();
    external_inputs.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    let mut external_outputs = devices
        .iter()
        .filter(|device| matches!(device.kind, NodeKind::ExternalOutput))
        .map(|device| RenderEndpoint {
            node_id: device.id.clone(),
            uid: device.uid.clone(),
            channels: device.channels,
        })
        .collect::<Vec<_>>();
    external_outputs.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    Some(RenderRuntimeConfig {
        sample_rate,
        buffer_frames,
        vout_sources,
        vin_sinks,
        external_inputs,
        external_outputs,
    })
}

fn profile_runtime_signature(profile: &Profile) -> BTreeMap<String, (String, u16, NodeKind)> {
    let mut signature = BTreeMap::new();
    let default_channels = profile.audio.channels.as_value().unwrap_or(2);

    for output in &profile.virtual_devices.outputs {
        signature.insert(
            output.id.clone(),
            (
                output
                    .uid
                    .clone()
                    .unwrap_or_else(|| format!("{MANAGED_UID_PREFIX}vout.{}", output.id)),
                output.channels.unwrap_or(default_channels),
                NodeKind::VirtualOutput,
            ),
        );
    }

    for input in &profile.virtual_devices.inputs {
        signature.insert(
            input.id.clone(),
            (
                input
                    .uid
                    .clone()
                    .unwrap_or_else(|| format!("{MANAGED_UID_PREFIX}vin.{}", input.id)),
                input.channels.unwrap_or(default_channels),
                NodeKind::VirtualInput,
            ),
        );
    }

    signature
}

fn render_restart_required(previous: &Profile, next: &Profile) -> bool {
    if previous.audio.sample_rate.as_value() != next.audio.sample_rate.as_value() {
        return true;
    }
    if previous.audio.buffer_frames != next.audio.buffer_frames {
        return true;
    }
    profile_runtime_signature(previous) != profile_runtime_signature(next)
}

fn read_driver_version() -> Option<String> {
    mars_hal_client::get_applied_state()
        .ok()
        .map(|state| state.driver_version)
}

fn parse_major(version: &str) -> Option<u64> {
    version.split('.').next()?.parse::<u64>().ok()
}

fn ensure_driver_compatibility_for_apply() -> Result<(), String> {
    let installed = Path::new("/Library/Audio/Plug-Ins/HAL/mars.driver").exists();
    let loaded = if installed {
        mars_hal_client::is_driver_loaded()
    } else {
        false
    };
    let daemon_version = env!("CARGO_PKG_VERSION");
    let driver_version = read_driver_version();

    if is_driver_compatible(installed, loaded, driver_version.as_deref(), daemon_version) {
        Ok(())
    } else {
        Err(format!(
            "incompatible driver version: driver={:?}, daemon={}",
            driver_version, daemon_version
        ))
    }
}

fn feedback_risk_notes(graph: Option<&RoutingGraph>, devices: &[DeviceDescriptor]) -> Vec<String> {
    let Some(graph) = graph else {
        return Vec::new();
    };

    let mut external_inputs = BTreeMap::<String, Vec<String>>::new();
    let mut external_outputs = BTreeMap::<String, Vec<String>>::new();
    for device in devices {
        match device.kind {
            NodeKind::ExternalInput => external_inputs
                .entry(device.uid.clone())
                .or_default()
                .push(device.id.clone()),
            NodeKind::ExternalOutput => external_outputs
                .entry(device.uid.clone())
                .or_default()
                .push(device.id.clone()),
            _ => {}
        }
    }

    let mut notes = Vec::new();
    for (uid, input_ids) in external_inputs {
        let Some(output_ids) = external_outputs.get(&uid) else {
            continue;
        };
        if path_exists_between_nodes(graph, &input_ids, output_ids) {
            notes.push(format!(
                "potential feedback risk detected: external input/output share uid '{uid}' and graph routes input to output"
            ));
        }
    }

    notes
}

fn path_exists_between_nodes(graph: &RoutingGraph, starts: &[String], targets: &[String]) -> bool {
    let target_set = targets.iter().cloned().collect::<BTreeSet<_>>();
    let mut adjacency = BTreeMap::<String, Vec<String>>::new();
    for edge in &graph.edges {
        adjacency
            .entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }

    let mut queue = starts.iter().cloned().collect::<VecDeque<_>>();
    let mut visited = BTreeSet::<String>::new();
    while let Some(node) = queue.pop_front() {
        if !visited.insert(node.clone()) {
            continue;
        }
        if target_set.contains(&node) {
            return true;
        }
        if let Some(next_nodes) = adjacency.get(&node) {
            queue.extend(next_nodes.iter().cloned());
        }
    }
    false
}

fn driver_state_path() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "cannot determine home directory".to_string())?;
    Ok(home.join("Library/Caches/mars/driver_applied_state.json"))
}

fn capture_driver_state_snapshot(path: &Path) -> Result<DriverStateSnapshot, String> {
    let raw_json = if path.exists() {
        Some(fs::read_to_string(path).map_err(|error| {
            format!(
                "failed to read existing driver state cache {}: {error}",
                path.display()
            )
        })?)
    } else {
        None
    };

    // Keep raw cache even if parse fails so file-level rollback still works; in that case
    // HAL-level rollback may be limited because desired state cannot be reconstructed.
    let parsed_state =
        raw_json.as_deref().and_then(
            |raw| match serde_json::from_str::<DriverAppliedState>(raw) {
                Ok(state) => Some(state),
                Err(error) => {
                    warn!(
                        error = %error,
                        path = %path.display(),
                        "driver state cache is not parseable; HAL rollback may be limited"
                    );
                    None
                }
            },
        );

    Ok(DriverStateSnapshot {
        path: path.to_path_buf(),
        raw_json,
        parsed_state,
    })
}

fn hal_desired_from_driver_applied(state: &DriverAppliedState) -> HalDesiredState {
    HalDesiredState {
        driver_version: state.version.clone(),
        sample_rate: state.sample_rate,
        channels: state.channels,
        buffer_frames: state.buffer_frames,
        devices: state
            .devices
            .iter()
            .map(|device| HalDeviceState {
                id: device.id.clone(),
                uid: device.uid.clone(),
                name: device.name.clone(),
                kind: node_kind_to_hal_kind(device.kind),
                channels: device.channels,
                hidden: device.hidden,
            })
            .collect(),
    }
}

fn empty_hal_desired_state() -> HalDesiredState {
    HalDesiredState {
        driver_version: env!("CARGO_PKG_VERSION").to_string(),
        sample_rate: 48_000,
        channels: 2,
        buffer_frames: default_driver_buffer_frames(),
        devices: Vec::new(),
    }
}

fn apply_hal_desired_state(desired: &HalDesiredState) -> Result<(), String> {
    mars_hal_client::set_desired_state(desired).map_err(|e| format!("driver client error: {e}"))
}

fn restore_driver_state_snapshot(snapshot: &DriverStateSnapshot) -> Result<(), String> {
    let mut rollback_issue: Option<String> = None;

    // Restore HAL runtime first, then restore cache file, so live driver state is recovered
    // before persisted metadata is rewritten.
    match snapshot.parsed_state.as_ref() {
        Some(previous_state) => {
            apply_hal_desired_state(&hal_desired_from_driver_applied(previous_state))?
        }
        None if snapshot.raw_json.is_none() => {
            apply_hal_desired_state(&empty_hal_desired_state())?;
        }
        None => {
            rollback_issue = Some(
                "cannot rollback HAL state because previous driver cache is invalid".to_string(),
            );
        }
    }

    match snapshot.raw_json.as_ref() {
        Some(raw_json) => fs::write(&snapshot.path, raw_json).map_err(|error| {
            format!(
                "failed to restore previous driver state cache {}: {error}",
                snapshot.path.display()
            )
        })?,
        None => {
            if snapshot.path.exists() {
                fs::remove_file(&snapshot.path).map_err(|error| {
                    format!(
                        "failed to remove new driver state cache {}: {error}",
                        snapshot.path.display()
                    )
                })?;
            }
        }
    }

    if let Some(issue) = rollback_issue {
        return Err(issue);
    }

    Ok(())
}

fn stage_driver_state(
    profile: &Profile,
    graph: &RoutingGraph,
    sample_rate: u32,
    channels: u16,
) -> Result<(), String> {
    ensure_driver_available()?;

    let path = driver_state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let snapshot = capture_driver_state_snapshot(&path)?;

    let default_channels = profile.audio.channels.as_value().unwrap_or(channels);
    let mut devices = Vec::new();
    let mut hal_devices = Vec::new();

    for output in &profile.virtual_devices.outputs {
        let uid = output
            .uid
            .clone()
            .unwrap_or_else(|| format!("{MANAGED_UID_PREFIX}vout.{}", output.id));
        let channels = output.channels.unwrap_or(default_channels);

        devices.push(DriverDevice {
            id: output.id.clone(),
            uid: uid.clone(),
            name: output.name.clone(),
            kind: NodeKind::VirtualOutput,
            channels,
            hidden: output.hidden,
        });
        hal_devices.push(HalDeviceState {
            id: output.id.clone(),
            uid,
            name: output.name.clone(),
            kind: node_kind_to_hal_kind(NodeKind::VirtualOutput),
            channels,
            hidden: output.hidden,
        });
    }

    for input in &profile.virtual_devices.inputs {
        let uid = input
            .uid
            .clone()
            .unwrap_or_else(|| format!("{MANAGED_UID_PREFIX}vin.{}", input.id));
        let channels = input.channels.unwrap_or(default_channels);

        devices.push(DriverDevice {
            id: input.id.clone(),
            uid: uid.clone(),
            name: input.name.clone(),
            kind: NodeKind::VirtualInput,
            channels,
            hidden: false,
        });
        hal_devices.push(HalDeviceState {
            id: input.id.clone(),
            uid,
            name: input.name.clone(),
            kind: node_kind_to_hal_kind(NodeKind::VirtualInput),
            channels,
            hidden: false,
        });
    }

    let state = DriverAppliedState {
        version: env!("CARGO_PKG_VERSION").to_string(),
        devices,
        sample_rate,
        channels,
        buffer_frames: profile.audio.buffer_frames,
    };

    let desired = HalDesiredState {
        driver_version: env!("CARGO_PKG_VERSION").to_string(),
        sample_rate,
        channels,
        buffer_frames: profile.audio.buffer_frames,
        devices: hal_devices,
    };
    if let Err(error) = apply_hal_desired_state(&desired) {
        if let Err(rollback_error) = restore_driver_state_snapshot(&snapshot) {
            // Preserve the primary stage failure as return error; rollback failure is diagnostic.
            warn!(
                error = %rollback_error,
                "failed to rollback driver state after HAL stage failure"
            );
        }
        return Err(error);
    }

    let serialized = serde_json::to_string_pretty(&state).map_err(|error| error.to_string())?;
    if let Err(error) = fs::write(&path, serialized) {
        if let Err(rollback_error) = restore_driver_state_snapshot(&snapshot) {
            // Preserve the primary cache write failure as return error; rollback failure is diagnostic.
            warn!(
                error = %rollback_error,
                "failed to rollback driver state after cache write failure"
            );
        }
        return Err(format!(
            "failed to persist driver state cache {}: {error}",
            path.display()
        ));
    }

    debug!(pipe_count = graph.edges.len(), "driver state staged");
    Ok(())
}

fn clear_driver_state() -> Result<(), String> {
    ensure_driver_available()?;
    let path = driver_state_path()?;
    let snapshot = capture_driver_state_snapshot(&path)?;

    if let Err(error) = apply_hal_desired_state(&empty_hal_desired_state()) {
        if let Err(rollback_error) = restore_driver_state_snapshot(&snapshot) {
            // Preserve the primary clear failure as return error; rollback failure is diagnostic.
            warn!(
                error = %rollback_error,
                "failed to rollback driver state after clear HAL failure"
            );
        }
        return Err(error);
    }
    let _ = global_registry().remove_namespace("mars.");

    if path.exists() {
        fs::remove_file(&path).map_err(|error| {
            let rollback = restore_driver_state_snapshot(&snapshot);
            if let Err(rollback_error) = rollback {
                // Preserve the primary cache remove failure as return error; rollback failure is diagnostic.
                warn!(
                    error = %rollback_error,
                    "failed to rollback driver state after cache remove failure"
                );
            }
            format!(
                "failed to remove driver state cache {}: {error}",
                path.display()
            )
        })?;
    }

    Ok(())
}

fn ensure_driver_available() -> Result<(), String> {
    let bundle = Path::new("/Library/Audio/Plug-Ins/HAL/mars.driver");
    if !bundle.exists() {
        return Err("mars.driver is not installed; run scripts/install.sh".to_string());
    }

    if !mars_hal_client::is_driver_loaded() {
        return Err("mars.driver bundle exists but plugin is not loaded by coreaudiod; try: sudo killall -9 coreaudiod".to_string());
    }

    Ok(())
}

fn is_driver_compatible(
    driver_installed: bool,
    driver_loaded: bool,
    driver_version: Option<&str>,
    daemon_version: &str,
) -> bool {
    if !driver_installed || !driver_loaded {
        return false;
    }

    let Some(driver_major) = driver_version.and_then(parse_major) else {
        return false;
    };
    let Some(daemon_major) = parse_major(daemon_version) else {
        return false;
    };

    driver_major == daemon_major
}

pub fn setup_logging() -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let log_path = home.join("Library/Logs/mars");
    fs::create_dir_all(&log_path).context("cannot create log directory")?;

    let file_appender = tracing_appender::rolling::never(log_path, "marsd.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .try_init()
        .ok();

    Ok(guard)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use mars_types::{Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

    use super::{MarsDaemon, diff_profiles, is_driver_compatible};

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
}

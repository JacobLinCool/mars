#![forbid(unsafe_code)]
//! marsd daemon implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use chrono::Utc;
use mars_coreaudio::{CoreAudioError, list_device_inventory, resolve_externals};
use mars_engine::{Engine, EngineSnapshot};
use mars_graph::RoutingGraph;
use mars_hal::{
    DesiredState as HalDesiredState, HalDevice as HalDeviceState, HalError as MarsHalError,
    RuntimeStats as HalRuntimeStats,
};
use mars_ipc::{
    ApiError, DaemonRequest, DaemonResponse, LogRequest, LogResponse, RequestHandler, serve,
};
use mars_profile::{ValidatedProfile, load_profile, validate_only};
use mars_types::{
    ApplyMode, ApplyPlan, ApplyRequest, ApplyResult, DaemonStatus, DeviceDescriptor,
    DriverStatusSummary, ExitCode, MANAGED_UID_PREFIX, NodeKind, PlanChange, PlanChangeKind,
    PlanRequest, Profile, RuntimeCounters,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

#[derive(Debug)]
pub struct MarsDaemon {
    state: Mutex<DaemonState>,
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
}

#[derive(Debug, Clone)]
struct DriverStateSnapshot {
    path: PathBuf,
    raw_json: Option<String>,
    parsed_state: Option<DriverAppliedState>,
}

const fn default_driver_buffer_frames() -> u32 {
    256
}

impl MarsDaemon {
    #[must_use]
    pub fn new(log_path: PathBuf) -> Self {
        Self {
            state: Mutex::new(DaemonState::default()),
            log_path,
        }
    }

    pub async fn run(self: Arc<Self>, socket_path: &Path) -> Result<(), mars_ipc::IpcError> {
        serve(socket_path, self).await
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
        let validated = load_profile(Path::new(&request.profile_path)).map_err(|error| {
            ApiError::new(
                format!("profile validation failed: {error}"),
                ExitCode::InvalidInput,
            )
        })?;

        let plan_request = PlanRequest {
            profile_path: request.profile_path.clone(),
            no_delete: request.no_delete,
        };

        let plan = self.plan_internal(&plan_request, &validated)?;
        if request.dry_run {
            return Ok(ApplyResult {
                applied: false,
                plan,
                warnings: Vec::new(),
                errors: Vec::new(),
            });
        }

        let inventory = list_device_inventory().map_err(coreaudio_to_api_error)?;
        let resolution = resolve_externals(&validated.profile, &inventory);
        if !resolution.errors.is_empty() {
            return Err(ApiError::new(
                format!("missing external devices: {}", resolution.errors.join("; ")),
                ExitCode::MissingExternal,
            ));
        }

        // When --no-delete is set, merge devices from the previous profile that
        // would otherwise be removed so the HAL desired state retains them.
        let effective_profile = if request.no_delete {
            let prev = self.state.lock().current_profile.clone();
            if let Some(prev) = prev {
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

        let sample_rate = validated
            .profile
            .audio
            .sample_rate
            .as_value()
            .unwrap_or(48_000);
        let channels = validated.profile.audio.channels.as_value().unwrap_or(2);
        let apply_mode = validated.profile.policy.apply_mode;
        let mut warnings = plan.warnings.clone();

        let engine = Arc::new(Engine::new(EngineSnapshot {
            graph: validated.graph.clone(),
            sample_rate,
            buffer_frames: validated.profile.audio.buffer_frames,
        }));
        let devices = collect_devices(&validated.profile, &resolution.resolved);

        if let Err(error) =
            stage_driver_state(&effective_profile, &validated.graph, sample_rate, channels)
        {
            warn!(error = %error, "failed to stage driver state");
            if matches!(apply_mode, ApplyMode::Atomic) {
                return Err(ApiError::new(error, ExitCode::DriverUnavailable));
            }
            // best_effort keeps control-plane state moving and reports data-plane divergence.
            warnings.push(format!(
                "driver stage failed but apply continued due to best_effort: {error}"
            ));
        }

        let mut state = self.state.lock();

        state.current_profile_path = Some(request.profile_path);
        state.current_profile = Some(effective_profile);
        state.graph = Some(validated.graph.clone());
        state.engine = Some(engine);
        state.devices = devices;

        info!("apply transaction committed");

        Ok(ApplyResult {
            applied: true,
            plan,
            warnings,
            errors: Vec::new(),
        })
    }

    fn clear_internal(&self, keep_devices: bool) -> Result<ApplyResult, ApiError> {
        let mut state = self.state.lock();
        let previous = state.clone();

        state.engine = None;
        state.current_profile = None;
        state.current_profile_path = None;
        state.graph = None;

        if !keep_devices {
            state.devices.retain(|device| !device.managed);
            if let Err(error) = clear_driver_state() {
                *state = previous;
                return Err(ApiError::new(
                    format!("clear failed and rolled back: {error}"),
                    ExitCode::DriverUnavailable,
                ));
            }
        }

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
        let state = self.state.lock();
        let profile = state.current_profile.as_ref();
        let driver = driver_status_summary();
        let mut counters = state.counters.clone();
        if let Some(runtime) = driver_runtime_stats() {
            counters.underrun_count = runtime.underrun_count;
            counters.overrun_count = runtime.overrun_count;
            counters.xrun_count = runtime.xrun_count;
        }

        DaemonStatus {
            running: state.engine.is_some(),
            current_profile: state.current_profile_path.clone(),
            sample_rate: profile
                .and_then(|profile| profile.audio.sample_rate.as_value())
                .unwrap_or(48_000),
            buffer_frames: profile.map_or(256, |profile| profile.audio.buffer_frames),
            graph_pipe_count: state.graph.as_ref().map_or(0, |graph| graph.edges.len()),
            devices: state.devices.clone(),
            counters,
            driver,
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

        let lines = data
            .lines()
            .rev()
            .take(200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        Ok(LogResponse {
            lines,
            streaming: request.follow,
        })
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
                            format!("profile validation failed: {error}"),
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
            DaemonRequest::Doctor => {
                let report = doctor_report();
                Ok(DaemonResponse::Doctor(report))
            }
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

fn hal_error_to_string(error: MarsHalError) -> String {
    format!("driver state operation failed: {error}")
}

fn driver_status_summary() -> DriverStatusSummary {
    if std::env::var("MARS_ALLOW_DRIVER_STUB").as_deref() == Ok("1") {
        return driver_status_summary_stub();
    }
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

fn driver_status_summary_stub() -> DriverStatusSummary {
    let summary = mars_hal::configuration_summary();
    DriverStatusSummary {
        generation: summary.current_generation,
        request_count: summary.request_count,
        perform_count: summary.perform_count,
        applied_device_count: summary.applied_device_count,
        pending_change: summary.pending.is_some(),
    }
}

fn driver_runtime_stats() -> Option<HalRuntimeStats> {
    if std::env::var("MARS_ALLOW_DRIVER_STUB").as_deref() == Ok("1") {
        return driver_runtime_stats_stub();
    }
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

fn driver_runtime_stats_stub() -> Option<HalRuntimeStats> {
    let raw = mars_hal::runtime_stats_json().ok()?;
    serde_json::from_str::<HalRuntimeStats>(&raw).ok()
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
    if std::env::var("MARS_ALLOW_DRIVER_STUB").as_deref() == Ok("1") {
        return apply_hal_desired_state_stub(desired);
    }
    mars_hal_client::set_desired_state(desired).map_err(|e| format!("driver client error: {e}"))
}

fn apply_hal_desired_state_stub(desired: &HalDesiredState) -> Result<(), String> {
    let desired_json = serde_json::to_string(desired).map_err(|error| {
        format!(
            "failed to serialize desired HAL state for sample_rate={} channels={}: {error}",
            desired.sample_rate, desired.channels
        )
    })?;
    mars_hal::set_desired_state_json(&desired_json).map_err(hal_error_to_string)?;

    let generation =
        mars_hal::request_device_configuration_change().map_err(hal_error_to_string)?;
    match mars_hal::perform_device_configuration_change(generation) {
        Ok(_) => Ok(()),
        Err(MarsHalError::NoPendingConfigurationChange) => {
            debug!(generation, "driver configuration already converged");
            Ok(())
        }
        Err(error) => Err(hal_error_to_string(error)),
    }
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
        });
        hal_devices.push(HalDeviceState {
            id: output.id.clone(),
            uid,
            name: output.name.clone(),
            kind: node_kind_to_hal_kind(NodeKind::VirtualOutput),
            channels,
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
        });
        hal_devices.push(HalDeviceState {
            id: input.id.clone(),
            uid,
            name: input.name.clone(),
            kind: node_kind_to_hal_kind(NodeKind::VirtualInput),
            channels,
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
    if std::env::var("MARS_ALLOW_DRIVER_STUB").as_deref() == Ok("1") {
        return Ok(());
    }

    let bundle = Path::new("/Library/Audio/Plug-Ins/HAL/mars.driver");
    if !bundle.exists() {
        return Err("mars.driver is not installed; run scripts/install.sh or set MARS_ALLOW_DRIVER_STUB=1 for development".to_string());
    }

    if !mars_hal_client::is_driver_loaded() {
        return Err("mars.driver bundle exists but plugin is not loaded by coreaudiod; try: sudo killall -9 coreaudiod".to_string());
    }

    Ok(())
}

fn doctor_report() -> mars_types::DoctorReport {
    let driver_installed = Path::new("/Library/Audio/Plug-Ins/HAL/mars.driver").exists();
    let daemon_reachable = true;
    let driver_loaded = if driver_installed {
        mars_hal_client::is_driver_loaded()
    } else {
        false
    };
    let driver = driver_status_summary();
    let microphone_permission_ok = std::env::var("MARS_ASSUME_MIC_PERMISSION")
        .as_deref()
        .map(|value| value == "1")
        .unwrap_or(false);

    let mut notes = Vec::new();
    if !driver_installed {
        notes.push("driver not found at /Library/Audio/Plug-Ins/HAL/mars.driver".to_string());
    }
    if driver_installed && !driver_loaded {
        notes.push("mars.driver is installed but not loaded by coreaudiod; run: sudo killall -9 coreaudiod".to_string());
    }
    if !microphone_permission_ok {
        notes.push(
            "microphone permission status could not be verified automatically; check System Settings > Privacy & Security > Microphone".to_string(),
        );
    }
    if driver.pending_change {
        notes.push("driver has a pending configuration change".to_string());
    }

    mars_types::DoctorReport {
        driver_installed,
        driver_compatible: driver_installed,
        daemon_reachable,
        microphone_permission_ok,
        driver,
        notes,
    }
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
mod tests {
    use mars_types::{Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

    use super::diff_profiles;

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
}

#![forbid(unsafe_code)]
//! High-level async SDK for building applications and tools on top of MARS.
//!
//! `MarsClient` wraps the low-level IPC transport and exposes typed operations
//! matching the public daemon contract.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mars_ipc::{Command, DaemonRequest, DaemonResponse, IpcClient};
pub use mars_ipc::{IpcError, LogRequest, LogResponse};
pub use mars_types::{
    ApplyPlan, ApplyRequest, ApplyResult, CaptureProcessInfo, ClearRequest,
    DEFAULT_SOCKET_PATH_RELATIVE, DaemonStatus, DeviceInventory, DoctorReport, ExitCode,
    PlanRequest, ValidateRequest, ValidationReport,
};
use thiserror::Error;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyOptions {
    pub no_delete: bool,
    pub dry_run: bool,
    pub timeout_ms: u64,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            no_delete: false,
            dry_run: false,
            timeout_ms: 5_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum MarsClientError {
    #[error("ipc error: {0}")]
    Ipc(#[from] IpcError),

    #[cfg(feature = "default-socket-path")]
    #[error("cannot determine home directory for default MARS socket path")]
    HomeDirectoryUnavailable,

    #[error("profile path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),

    #[error("daemon returned unexpected response: expected {expected:?}, got {actual:?}")]
    UnexpectedResponse { expected: Command, actual: Command },
}

#[derive(Debug, Clone)]
pub struct MarsClient {
    socket_path: PathBuf,
    timeout: Duration,
    ipc: IpcClient,
}

impl MarsClient {
    #[must_use]
    pub fn new(socket_path: PathBuf, timeout: Duration) -> Self {
        Self {
            ipc: IpcClient::new(socket_path.clone(), timeout),
            socket_path,
            timeout,
        }
    }

    #[cfg(feature = "default-socket-path")]
    #[cfg_attr(docsrs, doc(cfg(feature = "default-socket-path")))]
    pub fn new_default(timeout: Duration) -> Result<Self, MarsClientError> {
        let socket_path = Self::default_socket_path()?;
        Ok(Self::new(socket_path, timeout))
    }

    #[cfg(feature = "default-socket-path")]
    #[cfg_attr(docsrs, doc(cfg(feature = "default-socket-path")))]
    pub fn default_socket_path() -> Result<PathBuf, MarsClientError> {
        let home = dirs::home_dir().ok_or(MarsClientError::HomeDirectoryUnavailable)?;
        Ok(home.join(DEFAULT_SOCKET_PATH_RELATIVE))
    }

    #[must_use]
    pub const fn default_timeout() -> Duration {
        DEFAULT_REQUEST_TIMEOUT
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    pub async fn ping(&self) -> Result<(), MarsClientError> {
        match self.ipc.send(DaemonRequest::Ping).await? {
            DaemonResponse::Pong => Ok(()),
            other => Err(unexpected_response(Command::Ping, &other)),
        }
    }

    pub async fn validate(
        &self,
        request: ValidateRequest,
    ) -> Result<ValidationReport, MarsClientError> {
        match self.ipc.send(DaemonRequest::Validate(request)).await? {
            DaemonResponse::Validate(report) => Ok(report),
            other => Err(unexpected_response(Command::Validate, &other)),
        }
    }

    pub async fn validate_profile(
        &self,
        profile_path: impl AsRef<Path>,
    ) -> Result<ValidationReport, MarsClientError> {
        self.validate(ValidateRequest {
            profile_path: path_to_utf8(profile_path.as_ref())?,
        })
        .await
    }

    pub async fn plan(&self, request: PlanRequest) -> Result<ApplyPlan, MarsClientError> {
        match self.ipc.send(DaemonRequest::Plan(request)).await? {
            DaemonResponse::Plan(plan) => Ok(plan),
            other => Err(unexpected_response(Command::Plan, &other)),
        }
    }

    pub async fn plan_profile(
        &self,
        profile_path: impl AsRef<Path>,
        no_delete: bool,
    ) -> Result<ApplyPlan, MarsClientError> {
        self.plan(PlanRequest {
            profile_path: path_to_utf8(profile_path.as_ref())?,
            no_delete,
        })
        .await
    }

    pub async fn apply(&self, request: ApplyRequest) -> Result<ApplyResult, MarsClientError> {
        match self.ipc.send(DaemonRequest::Apply(request)).await? {
            DaemonResponse::Apply(result) => Ok(result),
            other => Err(unexpected_response(Command::Apply, &other)),
        }
    }

    pub async fn apply_profile(
        &self,
        profile_path: impl AsRef<Path>,
        options: ApplyOptions,
    ) -> Result<ApplyResult, MarsClientError> {
        self.apply(ApplyRequest {
            profile_path: path_to_utf8(profile_path.as_ref())?,
            no_delete: options.no_delete,
            dry_run: options.dry_run,
            timeout_ms: options.timeout_ms,
        })
        .await
    }

    pub async fn clear(&self, keep_devices: bool) -> Result<ApplyResult, MarsClientError> {
        match self
            .ipc
            .send(DaemonRequest::Clear(ClearRequest { keep_devices }))
            .await?
        {
            DaemonResponse::Clear(result) => Ok(result),
            other => Err(unexpected_response(Command::Clear, &other)),
        }
    }

    pub async fn status(&self) -> Result<DaemonStatus, MarsClientError> {
        match self.ipc.send(DaemonRequest::Status).await? {
            DaemonResponse::Status(status) => Ok(status),
            other => Err(unexpected_response(Command::Status, &other)),
        }
    }

    pub async fn devices(&self) -> Result<DeviceInventory, MarsClientError> {
        match self.ipc.send(DaemonRequest::Devices).await? {
            DaemonResponse::Devices(devices) => Ok(devices),
            other => Err(unexpected_response(Command::Devices, &other)),
        }
    }

    pub async fn processes(&self) -> Result<Vec<CaptureProcessInfo>, MarsClientError> {
        match self.ipc.send(DaemonRequest::Processes).await? {
            DaemonResponse::Processes(processes) => Ok(processes),
            other => Err(unexpected_response(Command::Processes, &other)),
        }
    }

    pub async fn logs(&self, request: LogRequest) -> Result<LogResponse, MarsClientError> {
        match self.ipc.send(DaemonRequest::Logs(request)).await? {
            DaemonResponse::Logs(response) => Ok(response),
            other => Err(unexpected_response(Command::Logs, &other)),
        }
    }

    pub async fn logs_once(
        &self,
        cursor: Option<u64>,
        limit: Option<u32>,
    ) -> Result<LogResponse, MarsClientError> {
        self.logs(LogRequest {
            follow: false,
            cursor,
            limit,
        })
        .await
    }

    pub async fn doctor(&self) -> Result<DoctorReport, MarsClientError> {
        match self.ipc.send(DaemonRequest::Doctor).await? {
            DaemonResponse::Doctor(report) => Ok(report),
            other => Err(unexpected_response(Command::Doctor, &other)),
        }
    }
}

fn path_to_utf8(path: &Path) -> Result<String, MarsClientError> {
    path.to_str()
        .map(std::borrow::ToOwned::to_owned)
        .ok_or_else(|| MarsClientError::NonUtf8Path(path.to_path_buf()))
}

fn unexpected_response(expected: Command, actual: &DaemonResponse) -> MarsClientError {
    MarsClientError::UnexpectedResponse {
        expected,
        actual: response_command(actual),
    }
}

const fn response_command(response: &DaemonResponse) -> Command {
    match response {
        DaemonResponse::Pong => Command::Ping,
        DaemonResponse::Validate(_) => Command::Validate,
        DaemonResponse::Plan(_) => Command::Plan,
        DaemonResponse::Apply(_) => Command::Apply,
        DaemonResponse::Clear(_) => Command::Clear,
        DaemonResponse::Status(_) => Command::Status,
        DaemonResponse::Devices(_) => Command::Devices,
        DaemonResponse::Processes(_) => Command::Processes,
        DaemonResponse::Logs(_) => Command::Logs,
        DaemonResponse::Doctor(_) => Command::Doctor,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::Utc;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;
    use mars_types::{
        CaptureRuntimeStatus, DriverStatusSummary, ExternalRuntimeStatus, PluginHostRuntimeStatus,
        RuntimeCounters, SinkRuntimeStatus,
    };

    #[test]
    fn apply_options_defaults_match_cli_defaults() {
        let options = ApplyOptions::default();
        assert!(!options.no_delete);
        assert!(!options.dry_run);
        assert_eq!(options.timeout_ms, 5_000);
    }

    #[test]
    fn unexpected_response_includes_expected_and_actual_commands() {
        let error = unexpected_response(Command::Status, &DaemonResponse::Pong);
        assert!(matches!(
            error,
            MarsClientError::UnexpectedResponse {
                expected: Command::Status,
                actual: Command::Ping,
            }
        ));
    }

    #[test]
    fn response_command_maps_all_variants() {
        assert_eq!(response_command(&DaemonResponse::Pong), Command::Ping);
        assert_eq!(
            response_command(&DaemonResponse::Validate(ValidationReport {
                valid: true,
                warnings: Vec::new(),
                errors: Vec::new(),
            })),
            Command::Validate
        );
        assert_eq!(
            response_command(&DaemonResponse::Plan(ApplyPlan::default())),
            Command::Plan
        );
        assert_eq!(
            response_command(&DaemonResponse::Apply(ApplyResult {
                applied: true,
                plan: ApplyPlan::default(),
                warnings: Vec::new(),
                errors: Vec::new(),
            })),
            Command::Apply
        );
        assert_eq!(
            response_command(&DaemonResponse::Clear(ApplyResult {
                applied: false,
                plan: ApplyPlan::default(),
                warnings: Vec::new(),
                errors: Vec::new(),
            })),
            Command::Clear
        );
        assert_eq!(
            response_command(&DaemonResponse::Status(DaemonStatus {
                running: false,
                current_profile: None,
                sample_rate: 48_000,
                buffer_frames: 256,
                graph_pipe_count: 0,
                graph_route_count: 0,
                devices: Vec::new(),
                counters: RuntimeCounters::default(),
                processor_runtime: std::collections::BTreeMap::new(),
                driver: DriverStatusSummary::default(),
                external_runtime: ExternalRuntimeStatus::default(),
                capture_runtime: CaptureRuntimeStatus::default(),
                sink_runtime: SinkRuntimeStatus::default(),
                plugin_runtime: PluginHostRuntimeStatus::default(),
                updated_at: Utc::now(),
            })),
            Command::Status
        );
        assert_eq!(
            response_command(&DaemonResponse::Devices(DeviceInventory {
                inputs: Vec::new(),
                outputs: Vec::new(),
            })),
            Command::Devices
        );
        assert_eq!(
            response_command(&DaemonResponse::Processes(Vec::new())),
            Command::Processes
        );
        assert_eq!(
            response_command(&DaemonResponse::Logs(LogResponse {
                lines: Vec::new(),
                next_cursor: 0,
            })),
            Command::Logs
        );
        assert_eq!(
            response_command(&DaemonResponse::Doctor(DoctorReport {
                driver_installed: true,
                driver_compatible: true,
                daemon_reachable: true,
                microphone_permission_ok: true,
                driver_version: Some("0.1.0".to_string()),
                daemon_version: "0.1.0".to_string(),
                mic_permission_source: "test".to_string(),
                driver: DriverStatusSummary::default(),
                capture_tap_supported: true,
                capture_active_taps: 0,
                capture_failed_taps: 0,
                sink_active: 0,
                sink_degraded: 0,
                sink_failed: 0,
                sink_write_errors: 0,
                plugin_active: 0,
                plugin_failed: 0,
                plugin_timeouts: 0,
                plugin_errors: 0,
                plugin_restarts: 0,
                notes: Vec::new(),
            })),
            Command::Doctor
        );
    }

    #[cfg(feature = "default-socket-path")]
    #[test]
    fn default_socket_path_uses_known_relative_location() {
        if let Ok(socket_path) = MarsClient::default_socket_path() {
            assert!(socket_path.ends_with(DEFAULT_SOCKET_PATH_RELATIVE));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_is_rejected() {
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(vec![0x66, 0x6f, 0x80]));
        let error = path_to_utf8(&path).unwrap_err();

        assert!(matches!(error, MarsClientError::NonUtf8Path(_)));
    }
}

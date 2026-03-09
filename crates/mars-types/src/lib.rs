#![forbid(unsafe_code)]
//! Shared types for MARS CLI, daemon, and supporting crates.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROFILE_VERSION: u32 = 1;
pub const PROFILE_FILE_EXTENSION: &str = "yaml";
pub const MANAGED_UID_PREFIX: &str = "com.mars.";
pub const MANUFACTURER_MARS: &str = "MARS";
pub const DEFAULT_PROFILE_DIR_RELATIVE: &str = "Library/Application Support/mars/profiles";
pub const DEFAULT_SOCKET_PATH_RELATIVE: &str = "Library/Caches/mars/marsd.sock";
pub const DEFAULT_LOG_PATH_RELATIVE: &str = "Library/Logs/mars/marsd.log";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    InvalidInput = 2,
    MissingExternal = 3,
    DriverUnavailable = 4,
    DaemonCommunication = 5,
    ApplyFailed = 6,
    Interrupted = 130,
}

impl ExitCode {
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Error)]
pub enum ExitCodeError {
    #[error("unsupported exit code: {0}")]
    Unsupported(i32),
}

impl TryFrom<i32> for ExitCode {
    type Error = ExitCodeError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        let mapped = match value {
            0 => Self::Success,
            2 => Self::InvalidInput,
            3 => Self::MissingExternal,
            4 => Self::DriverUnavailable,
            5 => Self::DaemonCommunication,
            6 => Self::ApplyFailed,
            130 => Self::Interrupted,
            _ => return Err(ExitCodeError::Unsupported(value)),
        };
        Ok(mapped)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Profile {
    pub version: u32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(rename = "virtual", default)]
    pub virtual_devices: VirtualDevices,
    #[serde(default)]
    pub buses: Vec<Bus>,
    #[serde(default)]
    pub external: ExternalDevices,
    #[serde(default)]
    pub pipes: Vec<Pipe>,
    #[serde(default)]
    pub policy: Policy,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: PROFILE_VERSION,
            name: None,
            description: None,
            audio: AudioConfig::default(),
            virtual_devices: VirtualDevices::default(),
            buses: Vec::new(),
            external: ExternalDevices::default(),
            pipes: Vec::new(),
            policy: Policy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct AudioConfig {
    #[serde(default = "default_sample_rate")]
    pub sample_rate: AutoOrU32,
    #[serde(default = "default_channels")]
    pub channels: AutoOrU16,
    #[serde(default = "default_buffer_frames")]
    pub buffer_frames: u32,
    #[serde(default)]
    pub format: AudioFormat,
    #[serde(default)]
    pub latency_mode: LatencyMode,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            buffer_frames: default_buffer_frames(),
            format: AudioFormat::default(),
            latency_mode: LatencyMode::default(),
        }
    }
}

const fn default_buffer_frames() -> u32 {
    256
}

fn default_sample_rate() -> AutoOrU32 {
    AutoOrU32::Value(48_000)
}

fn default_channels() -> AutoOrU16 {
    AutoOrU16::Value(2)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(untagged)]
pub enum AutoOrU32 {
    Value(u32),
    Auto(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(untagged)]
pub enum AutoOrU16 {
    Value(u16),
    Auto(String),
}

impl AutoOrU32 {
    #[must_use]
    pub fn as_value(&self) -> Option<u32> {
        match self {
            Self::Value(value) => Some(*value),
            Self::Auto(_) => None,
        }
    }
}

impl AutoOrU16 {
    #[must_use]
    pub fn as_value(&self) -> Option<u16> {
        match self {
            Self::Value(value) => Some(*value),
            Self::Auto(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    #[default]
    F32,
    I16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LatencyMode {
    Low,
    #[default]
    Balanced,
    Safe,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct VirtualDevices {
    #[serde(default)]
    pub outputs: Vec<VirtualOutputDevice>,
    #[serde(default)]
    pub inputs: Vec<VirtualInputDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct VirtualOutputDevice {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub channels: Option<u16>,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct VirtualInputDevice {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub channels: Option<u16>,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub mix: Option<MixConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Bus {
    pub id: String,
    #[serde(default)]
    pub channels: Option<u16>,
    #[serde(default)]
    pub mix: Option<MixConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct MixConfig {
    #[serde(default)]
    pub limiter: bool,
    #[serde(default = "default_limit_dbfs")]
    pub limit_dbfs: f32,
    #[serde(default)]
    pub mode: MixMode,
}

const fn default_limit_dbfs() -> f32 {
    -1.0
}

impl Default for MixConfig {
    fn default() -> Self {
        Self {
            limiter: false,
            limit_dbfs: default_limit_dbfs(),
            mode: MixMode::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MixMode {
    #[default]
    Sum,
    Average,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct ExternalDevices {
    #[serde(default)]
    pub inputs: Vec<ExternalInput>,
    #[serde(default)]
    pub outputs: Vec<ExternalOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExternalInput {
    pub id: String,
    pub r#match: DeviceMatch,
    #[serde(default)]
    pub channels: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExternalOutput {
    pub id: String,
    pub r#match: DeviceMatch,
    #[serde(default)]
    pub channels: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct DeviceMatch {
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub name_regex: Option<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub transport: Option<TransportType>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TransportType {
    Usb,
    Bluetooth,
    BuiltIn,
    Virtual,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Pipe {
    pub from: String,
    pub to: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub delay_ms: f32,
}

const fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct Policy {
    #[serde(default)]
    pub on_missing_external: MissingExternalPolicy,
    #[serde(default)]
    pub apply_mode: ApplyMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MissingExternalPolicy {
    #[default]
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApplyMode {
    #[default]
    Atomic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    VirtualOutput,
    VirtualInput,
    ExternalInput,
    ExternalOutput,
    Bus,
}

impl NodeKind {
    #[must_use]
    pub const fn is_source(self) -> bool {
        matches!(self, Self::VirtualOutput | Self::ExternalInput | Self::Bus)
    }

    #[must_use]
    pub const fn is_sink(self) -> bool {
        matches!(self, Self::VirtualInput | Self::ExternalOutput | Self::Bus)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct NodeDescriptor {
    pub id: String,
    pub kind: NodeKind,
    pub channels: u16,
    #[serde(default)]
    pub mix: Option<MixConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct PipeDescriptor {
    pub id: String,
    pub from: String,
    pub to: String,
    pub gain_db: f32,
    pub mute: bool,
    pub pan: f32,
    pub delay_ms: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanChangeKind {
    CreateDevice,
    UpdateDevice,
    RemoveDevice,
    UpdateGraph,
    UpdateAudioConfig,
    NoOp,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct PlanChange {
    pub kind: PlanChangeKind,
    pub target: String,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct ApplyPlan {
    #[serde(default)]
    pub changes: Vec<PlanChange>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ApplyResult {
    pub applied: bool,
    pub plan: ApplyPlan,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct RuntimeCounters {
    pub underrun_count: u64,
    pub overrun_count: u64,
    pub xrun_count: u64,
    pub deadline_miss_count: u64,
    #[serde(default)]
    pub last_callback_ns: u64,
    #[serde(default)]
    pub last_cycle_ns: u64,
    #[serde(default)]
    pub max_cycle_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
pub struct DriverStatusSummary {
    pub generation: u64,
    pub request_count: u64,
    pub perform_count: u64,
    pub applied_device_count: usize,
    pub pending_change: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
pub struct ExternalRuntimeStatus {
    pub connected_inputs: usize,
    pub connected_outputs: usize,
    #[serde(default)]
    pub degraded_inputs: usize,
    #[serde(default)]
    pub degraded_outputs: usize,
    pub restart_attempts: u64,
    #[serde(default)]
    pub stream_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct DeviceDescriptor {
    pub id: String,
    pub name: String,
    pub uid: String,
    pub kind: NodeKind,
    pub channels: u16,
    pub managed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct DaemonStatus {
    pub running: bool,
    #[serde(default)]
    pub current_profile: Option<String>,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    pub graph_pipe_count: usize,
    #[serde(default)]
    pub devices: Vec<DeviceDescriptor>,
    pub counters: RuntimeCounters,
    #[serde(default)]
    pub driver: DriverStatusSummary,
    #[serde(default)]
    pub external_runtime: ExternalRuntimeStatus,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct DoctorReport {
    pub driver_installed: bool,
    pub driver_compatible: bool,
    pub daemon_reachable: bool,
    #[serde(default)]
    pub microphone_permission_ok: bool,
    #[serde(default)]
    pub driver_version: Option<String>,
    pub daemon_version: String,
    #[serde(default)]
    pub mic_permission_source: String,
    #[serde(default)]
    pub driver: DriverStatusSummary,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct DeviceInventory {
    #[serde(default)]
    pub inputs: Vec<ExternalDeviceInfo>,
    #[serde(default)]
    pub outputs: Vec<ExternalDeviceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ExternalDeviceInfo {
    pub uid: String,
    pub name: String,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub transport: Option<TransportType>,
    pub channels: u16,
    #[serde(default)]
    pub sample_rates: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ResolvedExternalDevice {
    pub logical_id: String,
    pub matched_uid: String,
    pub name: String,
    pub kind: NodeKind,
    pub channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ApplyRequest {
    pub profile_path: String,
    pub no_delete: bool,
    pub dry_run: bool,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct PlanRequest {
    pub profile_path: String,
    pub no_delete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ValidateRequest {
    pub profile_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ClearRequest {
    pub keep_devices: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ValidationReport {
    pub valid: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}

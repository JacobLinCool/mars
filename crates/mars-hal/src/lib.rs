#![allow(unsafe_code)]
//! AudioServerPlugIn driver crate for MARS.
//!
//! Unsafe operations are intentionally concentrated in this crate.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROPERTY_DESIRED_STATE: &str = "com.mars.profile.desired_state";
pub const PROPERTY_APPLIED_STATE: &str = "com.mars.profile.applied_state";
pub const PROPERTY_RUNTIME_STATS: &str = "com.mars.runtime.stats";
pub const DRIVER_INTERFACE_ABI_VERSION: u32 = 1;

pub mod coreaudio_types;
pub mod plugin;
pub mod shm_backend;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DesiredState {
    pub driver_version: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    pub devices: Vec<HalDevice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppliedState {
    pub driver_version: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    pub devices: Vec<HalDevice>,
    pub shm_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeStats {
    pub underrun_count: u64,
    pub overrun_count: u64,
    pub xrun_count: u64,
    pub last_callback_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HalDevice {
    pub id: String,
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub channels: u16,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigChangeKind {
    CreateDevice,
    UpdateDevice,
    RemoveDevice,
    UpdateAudioConfig,
    NoOp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigChange {
    pub kind: ConfigChangeKind,
    pub target: String,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingConfigurationChange {
    pub generation: u64,
    pub created_at_ms: u64,
    pub changes: Vec<ConfigChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigurationSummary {
    pub current_generation: u64,
    pub request_count: u64,
    pub perform_count: u64,
    pub applied_device_count: usize,
    pub pending: Option<PendingConfigurationChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigChangeResult {
    pub applied: bool,
    pub generation: u64,
    pub changes: Vec<ConfigChange>,
}

#[derive(Debug, Error)]
pub enum HalError {
    #[error("invalid desired state json: {0}")]
    InvalidDesiredState(serde_json::Error),
    #[error("serialize failed: {0}")]
    Serialize(serde_json::Error),
    #[error("no desired state staged")]
    NoDesiredState,
    #[error("no pending configuration change")]
    NoPendingConfigurationChange,
    #[error("configuration generation mismatch: expected {expected}, got {actual}")]
    GenerationMismatch { expected: u64, actual: u64 },
    #[error("invalid generation value: {0}")]
    InvalidGeneration(i64),
}

#[derive(Debug, Clone)]
struct PendingChangeInternal {
    generation: u64,
    created_at_ms: u64,
    changes: Vec<ConfigChange>,
    desired_state: DesiredState,
}

#[derive(Debug)]
pub(crate) struct DriverState {
    pub(crate) desired_state: Option<DesiredState>,
    pub(crate) applied_state: AppliedState,
    pub(crate) runtime: RuntimeStats,
    pub(crate) pending_change: Option<PendingChangeInternal>,
    pub(crate) current_generation: u64,
    pub(crate) request_count: u64,
    pub(crate) perform_count: u64,
}

impl Default for DriverState {
    fn default() -> Self {
        Self {
            desired_state: None,
            applied_state: AppliedState {
                driver_version: env!("CARGO_PKG_VERSION").to_string(),
                sample_rate: 48_000,
                channels: 2,
                buffer_frames: 256,
                devices: Vec::new(),
                shm_names: Vec::new(),
            },
            runtime: RuntimeStats {
                underrun_count: 0,
                overrun_count: 0,
                xrun_count: 0,
                last_callback_ns: 0,
            },
            pending_change: None,
            current_generation: 0,
            request_count: 0,
            perform_count: 0,
        }
    }
}

pub(crate) static DRIVER_STATE: Lazy<Mutex<DriverState>> =
    Lazy::new(|| Mutex::new(DriverState::default()));

pub fn set_desired_state_json(raw: &str) -> Result<(), HalError> {
    let desired =
        serde_json::from_str::<DesiredState>(raw).map_err(HalError::InvalidDesiredState)?;

    let mut state = DRIVER_STATE.lock();
    let changes = build_change_plan(&state.applied_state, &desired);
    state.desired_state = Some(desired.clone());

    if changes.len() == 1 && changes[0].kind == ConfigChangeKind::NoOp {
        state.pending_change = None;
    } else {
        let generation = state.current_generation.saturating_add(1);
        state.pending_change = Some(PendingChangeInternal {
            generation,
            created_at_ms: epoch_millis(),
            changes,
            desired_state: desired,
        });
    }

    Ok(())
}

pub fn request_device_configuration_change() -> Result<u64, HalError> {
    let mut state = DRIVER_STATE.lock();
    if state.desired_state.is_none() {
        return Err(HalError::NoDesiredState);
    }

    state.request_count = state.request_count.saturating_add(1);

    if let Some(pending) = state.pending_change.as_ref() {
        Ok(pending.generation)
    } else {
        // No pending diff: callers can treat current generation as a converged token.
        Ok(state.current_generation)
    }
}

pub fn perform_device_configuration_change(
    generation: u64,
) -> Result<ConfigChangeResult, HalError> {
    let mut state = DRIVER_STATE.lock();
    // Returning NoPendingConfigurationChange makes no-op requests explicit to callers.
    let pending = state
        .pending_change
        .clone()
        .ok_or(HalError::NoPendingConfigurationChange)?;

    if pending.generation != generation {
        return Err(HalError::GenerationMismatch {
            expected: pending.generation,
            actual: generation,
        });
    }

    state.applied_state = applied_from_desired(&pending.desired_state);
    state.pending_change = None;
    state.current_generation = pending.generation;
    state.perform_count = state.perform_count.saturating_add(1);

    Ok(ConfigChangeResult {
        applied: true,
        generation: pending.generation,
        changes: pending.changes,
    })
}

pub fn pending_change() -> Option<PendingConfigurationChange> {
    let state = DRIVER_STATE.lock();
    state.pending_change.as_ref().map(public_pending)
}

pub fn pending_change_json() -> Result<String, HalError> {
    serde_json::to_string(&pending_change()).map_err(HalError::Serialize)
}

pub fn configuration_summary() -> ConfigurationSummary {
    let state = DRIVER_STATE.lock();
    ConfigurationSummary {
        current_generation: state.current_generation,
        request_count: state.request_count,
        perform_count: state.perform_count,
        applied_device_count: state.applied_state.devices.len(),
        pending: state.pending_change.as_ref().map(public_pending),
    }
}

pub fn configuration_summary_json() -> Result<String, HalError> {
    serde_json::to_string(&configuration_summary()).map_err(HalError::Serialize)
}

pub fn applied_state_json() -> Result<String, HalError> {
    let state = DRIVER_STATE.lock();
    serde_json::to_string(&state.applied_state).map_err(HalError::Serialize)
}

pub fn runtime_stats_json() -> Result<String, HalError> {
    let state = DRIVER_STATE.lock();
    serde_json::to_string(&state.runtime).map_err(HalError::Serialize)
}

pub fn applied_devices() -> Vec<HalDevice> {
    let state = DRIVER_STATE.lock();
    state.applied_state.devices.clone()
}

pub fn applied_device_count() -> usize {
    let state = DRIVER_STATE.lock();
    state.applied_state.devices.len()
}

fn build_change_plan(applied: &AppliedState, desired: &DesiredState) -> Vec<ConfigChange> {
    let mut changes = Vec::new();

    if applied.sample_rate != desired.sample_rate
        || applied.channels != desired.channels
        || applied.buffer_frames != desired.buffer_frames
    {
        changes.push(ConfigChange {
            kind: ConfigChangeKind::UpdateAudioConfig,
            target: "audio".to_string(),
            details: format!(
                "sample_rate {} -> {}, channels {} -> {}, buffer_frames {} -> {}",
                applied.sample_rate,
                desired.sample_rate,
                applied.channels,
                desired.channels,
                applied.buffer_frames,
                desired.buffer_frames
            ),
        });
    }

    let mut applied_by_uid = BTreeMap::<&str, &HalDevice>::new();
    for device in &applied.devices {
        applied_by_uid.insert(device.uid.as_str(), device);
    }

    let mut desired_by_uid = BTreeMap::<&str, &HalDevice>::new();
    for device in &desired.devices {
        desired_by_uid.insert(device.uid.as_str(), device);
    }

    for (uid, device) in &desired_by_uid {
        match applied_by_uid.get(uid) {
            None => changes.push(ConfigChange {
                kind: ConfigChangeKind::CreateDevice,
                target: (*uid).to_string(),
                details: format!("create {} ({})", device.name, normalize_kind(&device.kind)),
            }),
            Some(existing) => {
                if *existing != *device {
                    changes.push(ConfigChange {
                        kind: ConfigChangeKind::UpdateDevice,
                        target: (*uid).to_string(),
                        details: format!("update {} -> {}", existing.name, device.name),
                    });
                }
            }
        }
    }

    for (uid, existing) in &applied_by_uid {
        if !desired_by_uid.contains_key(uid) {
            changes.push(ConfigChange {
                kind: ConfigChangeKind::RemoveDevice,
                target: (*uid).to_string(),
                details: format!(
                    "remove {} ({})",
                    existing.name,
                    normalize_kind(&existing.kind)
                ),
            });
        }
    }

    if changes.is_empty() {
        changes.push(ConfigChange {
            kind: ConfigChangeKind::NoOp,
            target: "state".to_string(),
            details: "already converged".to_string(),
        });
    }

    changes
}

fn applied_from_desired(desired: &DesiredState) -> AppliedState {
    AppliedState {
        driver_version: desired.driver_version.clone(),
        sample_rate: desired.sample_rate,
        channels: desired.channels,
        buffer_frames: desired.buffer_frames,
        // Keep paired ring names for each logical device to reserve output/input paths.
        shm_names: desired
            .devices
            .iter()
            .flat_map(|device| {
                [
                    format!("mars.vout.{}", device.uid),
                    format!("mars.vin.{}", device.uid),
                ]
            })
            .collect(),
        devices: desired.devices.clone(),
    }
}

fn public_pending(internal: &PendingChangeInternal) -> PendingConfigurationChange {
    PendingConfigurationChange {
        generation: internal.generation,
        created_at_ms: internal.created_at_ms,
        changes: internal.changes.clone(),
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn normalize_kind(kind: &str) -> String {
    kind.trim().to_lowercase().replace(' ', "_")
}

pub mod ffi {
    //! FFI boundary for AudioServerPlugIn host integration.

    use std::ffi::{CStr, c_char};

    use super::{
        DRIVER_INTERFACE_ABI_VERSION, HalError, applied_device_count, applied_state_json,
        configuration_summary_json, pending_change_json, perform_device_configuration_change,
        request_device_configuration_change, runtime_stats_json, set_desired_state_json,
    };

    #[derive(Debug)]
    #[repr(C)]
    pub struct MarsAudioServerPlugInInterface {
        pub abi_version: u32,
        pub set_desired_state_json: unsafe extern "C" fn(*const c_char) -> i32,
        pub request_device_configuration_change: unsafe extern "C" fn() -> i64,
        pub perform_device_configuration_change: unsafe extern "C" fn(i64) -> i32,
        pub get_applied_state_json: unsafe extern "C" fn(*mut c_char, usize) -> isize,
        pub get_runtime_stats_json: unsafe extern "C" fn(*mut c_char, usize) -> isize,
        pub get_pending_change_json: unsafe extern "C" fn(*mut c_char, usize) -> isize,
        pub get_configuration_summary_json: unsafe extern "C" fn(*mut c_char, usize) -> isize,
        pub get_applied_device_count: unsafe extern "C" fn() -> usize,
    }

    static DRIVER_INTERFACE: MarsAudioServerPlugInInterface = MarsAudioServerPlugInInterface {
        abi_version: DRIVER_INTERFACE_ABI_VERSION,
        set_desired_state_json: mars_hal_set_desired_state_json,
        request_device_configuration_change: mars_hal_request_device_configuration_change,
        perform_device_configuration_change: mars_hal_perform_device_configuration_change,
        get_applied_state_json: mars_hal_get_applied_state_json,
        get_runtime_stats_json: mars_hal_get_runtime_stats_json,
        get_pending_change_json: mars_hal_get_pending_change_json,
        get_configuration_summary_json: mars_hal_get_configuration_summary_json,
        get_applied_device_count: mars_hal_get_applied_device_count,
    };

    /// Accessor for tests/tooling that want a strongly typed interface pointer.
    ///
    /// # Safety
    /// Returned pointer references static storage and must not be freed or
    /// mutated by the caller.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_get_driver_interface() -> *const MarsAudioServerPlugInInterface
    {
        &DRIVER_INTERFACE
    }

    /// Set desired state from JSON payload.
    /// Returns 0 on success, non-zero on parse error.
    ///
    /// # Safety
    /// `raw` must be a valid, NUL-terminated C string pointer that remains
    /// alive for the duration of this call.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_set_desired_state_json(raw: *const c_char) -> i32 {
        if raw.is_null() {
            return 2;
        }

        // SAFETY: `raw` is checked for null above and expected to point to a
        // valid NUL-terminated C string owned by the caller for the duration of
        // this call.
        let c_str = unsafe { CStr::from_ptr(raw) };
        let Ok(raw) = c_str.to_str() else {
            return 3;
        };

        match set_desired_state_json(raw) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Trigger RequestDeviceConfigurationChange phase.
    /// Returns generation token on success, negative value on failure.
    ///
    /// # Safety
    /// C ABI entrypoint; no additional safety requirements beyond valid call
    /// convention.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_request_device_configuration_change() -> i64 {
        match request_device_configuration_change() {
            Ok(generation) => generation as i64,
            Err(_) => -1,
        }
    }

    /// Trigger PerformDeviceConfigurationChange phase.
    /// Returns 0 on success.
    /// Returns 2 if no pending change exists.
    /// Returns 3 if generation mismatches.
    /// Returns 4 for other failures.
    ///
    /// # Safety
    /// C ABI entrypoint; caller provides the generation token as returned by
    /// `mars_hal_request_device_configuration_change`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_perform_device_configuration_change(generation: i64) -> i32 {
        if generation < 0 {
            return 4;
        }

        match perform_device_configuration_change(generation as u64) {
            Ok(_) => 0,
            Err(HalError::NoPendingConfigurationChange) => 2,
            Err(HalError::GenerationMismatch { .. }) => 3,
            Err(_) => 4,
        }
    }

    /// Copy applied state JSON into caller-provided buffer.
    /// Returns number of bytes written (excluding trailing NUL) or -1 on error.
    ///
    /// # Safety
    /// `out` must point to a writable buffer of length `out_len`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_get_applied_state_json(
        out: *mut c_char,
        out_len: usize,
    ) -> isize {
        copy_json_to_buffer(out, out_len, applied_state_json)
    }

    /// Copy runtime stats JSON into caller-provided buffer.
    /// Returns number of bytes written (excluding trailing NUL) or -1 on error.
    ///
    /// # Safety
    /// `out` must point to a writable buffer of length `out_len`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_get_runtime_stats_json(
        out: *mut c_char,
        out_len: usize,
    ) -> isize {
        copy_json_to_buffer(out, out_len, runtime_stats_json)
    }

    /// Copy pending configuration change JSON into caller-provided buffer.
    /// Returns number of bytes written (excluding trailing NUL) or -1 on error.
    ///
    /// # Safety
    /// `out` must point to a writable buffer of length `out_len`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_get_pending_change_json(
        out: *mut c_char,
        out_len: usize,
    ) -> isize {
        copy_json_to_buffer(out, out_len, pending_change_json)
    }

    /// Copy configuration summary JSON into caller-provided buffer.
    /// Returns number of bytes written (excluding trailing NUL) or -1 on error.
    ///
    /// # Safety
    /// `out` must point to a writable buffer of length `out_len`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_get_configuration_summary_json(
        out: *mut c_char,
        out_len: usize,
    ) -> isize {
        copy_json_to_buffer(out, out_len, configuration_summary_json)
    }

    /// Return number of currently applied devices.
    ///
    /// # Safety
    /// C ABI entrypoint; no additional safety requirements beyond valid call
    /// convention.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mars_hal_get_applied_device_count() -> usize {
        applied_device_count()
    }

    fn copy_json_to_buffer(
        out: *mut c_char,
        out_len: usize,
        producer: impl FnOnce() -> Result<String, HalError>,
    ) -> isize {
        if out.is_null() || out_len == 0 {
            return -1;
        }

        let Ok(json) = producer() else {
            return -1;
        };

        let bytes = json.as_bytes();
        if bytes.len() + 1 > out_len {
            return -1;
        }

        // SAFETY: `out` is non-null and caller guarantees writable buffer of
        // size `out_len`. Source is a valid byte slice with no overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), bytes.len());
        }

        // SAFETY: `bytes.len() < out_len` is guaranteed above.
        unsafe {
            *out.add(bytes.len()) = 0;
        }

        bytes.len() as isize
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use once_cell::sync::Lazy;
    use parking_lot::Mutex;

    use super::{
        HalError, applied_state_json, configuration_summary, pending_change,
        perform_device_configuration_change, request_device_configuration_change,
        set_desired_state_json,
    };

    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    const SAMPLE_PAYLOAD: &str = r#"{
      "driver_version": "1.0.0",
      "sample_rate": 48000,
      "channels": 2,
      "buffer_frames": 256,
      "devices": [
        {
          "id": "mix-main",
          "uid": "com.mars.vin.mix-main",
          "name": "Mix Main",
          "kind": "virtual_input",
          "channels": 2
        }
      ]
    }"#;

    const EMPTY_PAYLOAD: &str = r#"{
      "driver_version": "1.0.0",
      "sample_rate": 48000,
      "channels": 2,
      "buffer_frames": 256,
      "devices": []
    }"#;

    fn reset_driver_state() {
        set_desired_state_json(EMPTY_PAYLOAD).expect("stage reset desired state");
        let generation = request_device_configuration_change().expect("request reset change");
        let _ = perform_device_configuration_change(generation);
    }

    #[test]
    fn staged_change_updates_applied_after_perform() {
        let _guard = TEST_LOCK.lock();
        reset_driver_state();
        set_desired_state_json(SAMPLE_PAYLOAD).expect("stage desired state");

        let generation = request_device_configuration_change().expect("request change");
        assert!(generation >= 1);
        assert!(pending_change().is_some());

        let result = perform_device_configuration_change(generation).expect("perform change");
        assert!(result.applied);
        assert!(result.generation >= 1);

        let applied = applied_state_json().expect("read applied state");
        assert!(applied.contains("mix-main"));
    }

    #[test]
    fn generation_mismatch_is_reported() {
        let _guard = TEST_LOCK.lock();
        reset_driver_state();
        set_desired_state_json(SAMPLE_PAYLOAD).expect("stage desired state");
        let generation = request_device_configuration_change().expect("request change");

        let err = perform_device_configuration_change(generation + 1).expect_err("must fail");
        assert!(matches!(err, HalError::GenerationMismatch { .. }));

        perform_device_configuration_change(generation).expect("apply with correct generation");
    }

    #[test]
    fn no_pending_change_returns_error() {
        let _guard = TEST_LOCK.lock();
        reset_driver_state();
        set_desired_state_json(EMPTY_PAYLOAD).expect("stage desired");
        let generation = request_device_configuration_change().expect("request change");

        // no pending change because desired == applied
        assert_eq!(generation, configuration_summary().current_generation);
        let err = perform_device_configuration_change(generation).expect_err("must fail");
        assert!(matches!(err, HalError::NoPendingConfigurationChange));
    }
}

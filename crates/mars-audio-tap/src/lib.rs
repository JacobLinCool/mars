//! CoreAudio tap control-plane primitives for macOS 15+.

#[cfg(not(target_os = "macos"))]
compile_error!("mars-audio-tap only supports macOS targets");

use std::ffi::{CStr, CString, c_char};

use serde::Deserialize;
use thiserror::Error;

const NO_ERR: i32 = 0;
const K_AUDIO_HARDWARE_BAD_OBJECT_ERROR: i32 = 0x216F626A; // '!obj'
const K_AUDIO_HARDWARE_BAD_DEVICE_ERROR: i32 = 0x21646576; // '!dev'

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapCapability {
    pub supported: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioProcessInfo {
    pub process_object_id: u32,
    pub pid: i32,
    pub bundle_id: String,
    pub is_running: bool,
    pub is_running_input: bool,
    pub is_running_output: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapHandle {
    pub tap_id: u32,
    pub tap_uid: String,
    pub aggregate_device_id: u32,
    pub aggregate_uid: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TapMuteBehavior {
    #[default]
    Unmuted,
    Muted,
    MutedWhenTapped,
}

impl TapMuteBehavior {
    const fn as_i32(self) -> i32 {
        match self {
            Self::Unmuted => 0,
            Self::Muted => 1,
            Self::MutedWhenTapped => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTapRequest {
    pub process_object_ids: Vec<u32>,
    pub tap_name: String,
    pub aggregate_uid: String,
    pub aggregate_name: String,
    pub mono: bool,
    pub private_tap: bool,
    pub mute_behavior: TapMuteBehavior,
    pub auto_start: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemTapTarget {
    DefaultOutput,
    AllOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemTapRequest {
    pub target: SystemTapTarget,
    pub tap_name: String,
    pub aggregate_uid: String,
    pub aggregate_name: String,
    pub mono: bool,
    pub private_tap: bool,
    pub mute_behavior: TapMuteBehavior,
    pub auto_start: bool,
    pub stream_index: i32,
}

#[derive(Debug, Error)]
pub enum TapError {
    #[error("invalid tap request: {0}")]
    InvalidArgument(String),
    #[error("tap backend reported unsupported capability: {0}")]
    Unsupported(String),
    #[error("coreaudio operation '{operation}' failed (status={status}): {message}")]
    CoreAudio {
        operation: &'static str,
        status: i32,
        message: String,
    },
    #[error("bridge decode failed: {0}")]
    Decode(String),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CoreAudioTapController;

impl CoreAudioTapController {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    pub fn capability(&self) -> Result<TapCapability, TapError> {
        let mut supported = 0_u8;
        let mut bridge_error = std::ptr::null_mut();
        let status = unsafe { mars_tap_check_capability(&mut supported, &mut bridge_error) };
        if status != NO_ERR {
            return Err(from_bridge_error("check_capability", status, bridge_error));
        }

        Ok(TapCapability {
            supported: supported != 0,
            reason: take_bridge_string(bridge_error),
        })
    }

    pub fn list_processes(&self) -> Result<Vec<AudioProcessInfo>, TapError> {
        let mut json_ptr = std::ptr::null_mut();
        let mut bridge_error = std::ptr::null_mut();
        let status = unsafe { mars_tap_list_processes_json(&mut json_ptr, &mut bridge_error) };
        if status != NO_ERR {
            return Err(from_bridge_error("list_processes", status, bridge_error));
        }

        let json = take_bridge_string(json_ptr)
            .ok_or_else(|| TapError::Decode("bridge returned empty process JSON".to_string()))?;

        let mut records: Vec<BridgeProcessInfo> =
            serde_json::from_str(&json).map_err(|error| TapError::Decode(error.to_string()))?;
        records.sort_by_key(|item| (item.pid, item.process_object_id));

        Ok(records
            .into_iter()
            .map(|item| AudioProcessInfo {
                process_object_id: item.process_object_id,
                pid: item.pid,
                bundle_id: item.bundle_id,
                is_running: item.is_running,
                is_running_input: item.is_running_input,
                is_running_output: item.is_running_output,
            })
            .collect())
    }

    pub fn default_output_device_uid(&self) -> Result<String, TapError> {
        let mut uid_ptr = std::ptr::null_mut();
        let mut bridge_error = std::ptr::null_mut();
        let status = unsafe { mars_tap_default_output_device_uid(&mut uid_ptr, &mut bridge_error) };
        if status != NO_ERR {
            return Err(from_bridge_error(
                "default_output_device_uid",
                status,
                bridge_error,
            ));
        }

        take_bridge_string(uid_ptr)
            .ok_or_else(|| TapError::Decode("bridge returned empty default output UID".to_string()))
    }

    pub fn create_process_tap(&self, request: &ProcessTapRequest) -> Result<TapHandle, TapError> {
        if request.process_object_ids.is_empty() {
            return Err(TapError::InvalidArgument(
                "process_tap requires at least one process object id".to_string(),
            ));
        }

        let tap_name = cstring(&request.tap_name, "tap_name")?;
        let aggregate_uid = cstring(&request.aggregate_uid, "aggregate_uid")?;
        let aggregate_name = cstring(&request.aggregate_name, "aggregate_name")?;

        let mut tap_id = 0_u32;
        let mut tap_uid_ptr = std::ptr::null_mut();
        let mut bridge_error = std::ptr::null_mut();
        let status = unsafe {
            mars_tap_create_process_tap(
                request.process_object_ids.as_ptr(),
                request.process_object_ids.len() as u32,
                0,
                u8::from(request.mono),
                tap_name.as_ptr(),
                u8::from(request.private_tap),
                request.mute_behavior.as_i32(),
                &mut tap_id,
                &mut tap_uid_ptr,
                &mut bridge_error,
            )
        };
        if status != NO_ERR {
            return Err(from_bridge_error(
                "create_process_tap",
                status,
                bridge_error,
            ));
        }

        let tap_uid = take_bridge_string(tap_uid_ptr)
            .ok_or_else(|| TapError::Decode("bridge returned empty tap UID".to_string()))?;

        let aggregate_device_id = match self.create_private_aggregate(
            &tap_uid,
            &aggregate_uid,
            &aggregate_name,
            request.auto_start,
        ) {
            Ok(aggregate_device_id) => aggregate_device_id,
            Err(error) => {
                self.best_effort_destroy_process_tap(tap_id);
                return Err(error);
            }
        };

        Ok(TapHandle {
            tap_id,
            tap_uid,
            aggregate_device_id,
            aggregate_uid: request.aggregate_uid.clone(),
        })
    }

    pub fn create_system_tap(&self, request: &SystemTapRequest) -> Result<TapHandle, TapError> {
        let tap_name = cstring(&request.tap_name, "tap_name")?;
        let aggregate_uid = cstring(&request.aggregate_uid, "aggregate_uid")?;
        let aggregate_name = cstring(&request.aggregate_name, "aggregate_name")?;

        let default_uid = match request.target {
            SystemTapTarget::AllOutput => None,
            SystemTapTarget::DefaultOutput => {
                let uid = self.default_output_device_uid()?;
                Some(cstring(&uid, "default_output_device_uid")?)
            }
        };
        let device_uid_ptr = default_uid
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr());

        let mut tap_id = 0_u32;
        let mut tap_uid_ptr = std::ptr::null_mut();
        let mut bridge_error = std::ptr::null_mut();
        let status = unsafe {
            mars_tap_create_system_tap(
                device_uid_ptr,
                request.stream_index,
                u8::from(request.mono),
                tap_name.as_ptr(),
                u8::from(request.private_tap),
                request.mute_behavior.as_i32(),
                &mut tap_id,
                &mut tap_uid_ptr,
                &mut bridge_error,
            )
        };
        if status != NO_ERR {
            return Err(from_bridge_error("create_system_tap", status, bridge_error));
        }

        let tap_uid = take_bridge_string(tap_uid_ptr)
            .ok_or_else(|| TapError::Decode("bridge returned empty tap UID".to_string()))?;

        let aggregate_device_id = match self.create_private_aggregate(
            &tap_uid,
            &aggregate_uid,
            &aggregate_name,
            request.auto_start,
        ) {
            Ok(aggregate_device_id) => aggregate_device_id,
            Err(error) => {
                self.best_effort_destroy_process_tap(tap_id);
                return Err(error);
            }
        };

        Ok(TapHandle {
            tap_id,
            tap_uid,
            aggregate_device_id,
            aggregate_uid: request.aggregate_uid.clone(),
        })
    }

    pub fn destroy_tap(&self, handle: &TapHandle) -> Result<(), TapError> {
        let mut aggregate_error = std::ptr::null_mut();
        let aggregate_status = unsafe {
            mars_tap_destroy_aggregate_device(handle.aggregate_device_id, &mut aggregate_error)
        };
        if aggregate_status != NO_ERR && !is_destroy_idempotent_status(aggregate_status) {
            return Err(from_bridge_error(
                "destroy_aggregate_device",
                aggregate_status,
                aggregate_error,
            ));
        }

        let mut tap_error = std::ptr::null_mut();
        let tap_status = unsafe { mars_tap_destroy_process_tap(handle.tap_id, &mut tap_error) };
        if tap_status != NO_ERR && !is_destroy_idempotent_status(tap_status) {
            return Err(from_bridge_error(
                "destroy_process_tap",
                tap_status,
                tap_error,
            ));
        }

        Ok(())
    }

    fn create_private_aggregate(
        &self,
        tap_uid: &str,
        aggregate_uid: &CString,
        aggregate_name: &CString,
        auto_start: bool,
    ) -> Result<u32, TapError> {
        let tap_uid_cstr = cstring(tap_uid, "tap_uid")?;
        let mut aggregate_device_id = 0_u32;
        let mut bridge_error = std::ptr::null_mut();
        let status = unsafe {
            mars_tap_create_private_aggregate_device(
                aggregate_uid.as_ptr(),
                aggregate_name.as_ptr(),
                tap_uid_cstr.as_ptr(),
                u8::from(auto_start),
                &mut aggregate_device_id,
                &mut bridge_error,
            )
        };

        if status != NO_ERR {
            return Err(from_bridge_error(
                "create_private_aggregate_device",
                status,
                bridge_error,
            ));
        }

        Ok(aggregate_device_id)
    }

    fn best_effort_destroy_process_tap(&self, tap_id: u32) {
        let mut bridge_error = std::ptr::null_mut();
        let _ = unsafe { mars_tap_destroy_process_tap(tap_id, &mut bridge_error) };
        let _ = take_bridge_string(bridge_error);
    }
}

#[derive(Debug, Deserialize)]
struct BridgeProcessInfo {
    process_object_id: u32,
    pid: i32,
    bundle_id: String,
    is_running: bool,
    is_running_input: bool,
    is_running_output: bool,
}

fn cstring(value: &str, field: &str) -> Result<CString, TapError> {
    CString::new(value).map_err(|_| {
        TapError::InvalidArgument(format!(
            "{field} contains an embedded NUL byte which is not supported"
        ))
    })
}

fn is_destroy_idempotent_status(status: i32) -> bool {
    status == K_AUDIO_HARDWARE_BAD_OBJECT_ERROR || status == K_AUDIO_HARDWARE_BAD_DEVICE_ERROR
}

fn from_bridge_error(operation: &'static str, status: i32, bridge_error: *mut c_char) -> TapError {
    let message = take_bridge_string(bridge_error)
        .unwrap_or_else(|| format!("{operation} failed without bridge error details"));
    TapError::CoreAudio {
        operation,
        status,
        message,
    }
}

fn take_bridge_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }

    let text = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { mars_tap_free_cstring(ptr) };
    Some(text)
}

unsafe extern "C" {
    fn mars_tap_free_cstring(value: *mut c_char);

    fn mars_tap_check_capability(out_supported: *mut u8, out_error: *mut *mut c_char) -> i32;

    fn mars_tap_list_processes_json(out_json: *mut *mut c_char, out_error: *mut *mut c_char)
    -> i32;

    fn mars_tap_default_output_device_uid(
        out_uid: *mut *mut c_char,
        out_error: *mut *mut c_char,
    ) -> i32;

    fn mars_tap_create_process_tap(
        process_ids: *const u32,
        process_count: u32,
        exclusive: u8,
        mono: u8,
        name: *const c_char,
        private_tap: u8,
        mute_behavior: i32,
        out_tap_id: *mut u32,
        out_tap_uid: *mut *mut c_char,
        out_error: *mut *mut c_char,
    ) -> i32;

    fn mars_tap_create_system_tap(
        device_uid: *const c_char,
        stream_index: i32,
        mono: u8,
        name: *const c_char,
        private_tap: u8,
        mute_behavior: i32,
        out_tap_id: *mut u32,
        out_tap_uid: *mut *mut c_char,
        out_error: *mut *mut c_char,
    ) -> i32;

    fn mars_tap_create_private_aggregate_device(
        aggregate_uid: *const c_char,
        aggregate_name: *const c_char,
        tap_uid: *const c_char,
        auto_start: u8,
        out_device_id: *mut u32,
        out_error: *mut *mut c_char,
    ) -> i32;

    fn mars_tap_destroy_process_tap(tap_id: u32, out_error: *mut *mut c_char) -> i32;

    fn mars_tap_destroy_aggregate_device(device_id: u32, out_error: *mut *mut c_char) -> i32;
}

#[cfg(test)]
mod tests {
    use super::is_destroy_idempotent_status;

    #[test]
    fn idempotent_destroy_status_recognizes_known_coreaudio_codes() {
        assert!(is_destroy_idempotent_status(0x216F626A));
        assert!(is_destroy_idempotent_status(0x2164_6576));
        assert!(!is_destroy_idempotent_status(0));
        assert!(!is_destroy_idempotent_status(-1));
    }
}

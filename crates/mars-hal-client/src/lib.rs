#![allow(unsafe_code)]
//! CoreAudio client library for communicating with the Mars driver plugin.
//!
//! This crate provides a safe Rust API that uses the CoreAudio client APIs
//! (`AudioObjectSetPropertyData` / `AudioObjectGetPropertyData`) to talk to
//! the Mars AudioServerPlugIn running inside coreaudiod.
//!
//! `mars-daemon` (which is `#![forbid(unsafe_code)]`) depends on this crate
//! instead of calling `mars_hal` functions directly.

mod bindings;

use mars_hal::coreaudio_types::{
    K_MARS_PROPERTY_APPLIED_STATE, K_MARS_PROPERTY_CONFIG_SUMMARY, K_MARS_PROPERTY_DESIRED_STATE,
    K_MARS_PROPERTY_RUNTIME_STATS,
};
use mars_hal::{AppliedState, ConfigurationSummary, DesiredState, RuntimeStats};

use bindings::{
    find_mars_plugin_id, get_property_cfdata, is_mars_plugin_loaded, set_property_cfdata,
};

#[derive(Debug, thiserror::Error)]
pub enum HalClientError {
    #[error("Mars driver plugin not found in loaded CoreAudio plugins")]
    DriverNotFound,

    #[error("CoreAudio OSStatus error: {0}")]
    OsStatus(i32),

    #[error("CoreFoundation error: {0}")]
    CoreFoundationError(String),

    #[error("JSON serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("invalid UTF-8 in property data")]
    InvalidUtf8,
}

/// Find the Mars driver plugin object ID.
///
/// Returns the `AudioObjectID` for the plugin, which can be used with other
/// functions in this crate.
pub fn find_mars_driver() -> Result<u32, HalClientError> {
    find_mars_plugin_id()
}

/// Send a desired state configuration to the Mars driver via CoreAudio IPC.
///
/// The JSON is sent as CFData through `AudioObjectSetPropertyData` on the
/// plugin's custom `kMarsPropertyDesiredState` property.  The driver receives
/// this in `SetPropertyData`, stages the change, and asks the host to call
/// `PerformDeviceConfigurationChange`.
pub fn set_desired_state(desired: &DesiredState) -> Result<(), HalClientError> {
    let json = serde_json::to_string(desired)?;
    let plugin_id = find_mars_plugin_id()?;
    set_property_cfdata(plugin_id, K_MARS_PROPERTY_DESIRED_STATE, json.as_bytes())
}

/// Read the currently applied state from the Mars driver via CoreAudio IPC.
pub fn get_applied_state() -> Result<AppliedState, HalClientError> {
    let plugin_id = find_mars_plugin_id()?;
    let bytes = get_property_cfdata(plugin_id, K_MARS_PROPERTY_APPLIED_STATE)?;
    let json_str = core::str::from_utf8(&bytes).map_err(|_| HalClientError::InvalidUtf8)?;
    let state: AppliedState = serde_json::from_str(json_str)?;
    Ok(state)
}

/// Read runtime statistics from the Mars driver via CoreAudio IPC.
pub fn get_runtime_stats() -> Result<RuntimeStats, HalClientError> {
    let plugin_id = find_mars_plugin_id()?;
    let bytes = get_property_cfdata(plugin_id, K_MARS_PROPERTY_RUNTIME_STATS)?;
    let json_str = core::str::from_utf8(&bytes).map_err(|_| HalClientError::InvalidUtf8)?;
    let stats: RuntimeStats = serde_json::from_str(json_str)?;
    Ok(stats)
}

/// Read the configuration summary from the Mars driver via CoreAudio IPC.
pub fn get_configuration_summary() -> Result<ConfigurationSummary, HalClientError> {
    let plugin_id = find_mars_plugin_id()?;
    let bytes = get_property_cfdata(plugin_id, K_MARS_PROPERTY_CONFIG_SUMMARY)?;
    let json_str = core::str::from_utf8(&bytes).map_err(|_| HalClientError::InvalidUtf8)?;
    let summary: ConfigurationSummary = serde_json::from_str(json_str)?;
    Ok(summary)
}

/// Check if the Mars driver plugin is loaded in coreaudiod.
pub fn is_driver_loaded() -> bool {
    is_mars_plugin_loaded()
}

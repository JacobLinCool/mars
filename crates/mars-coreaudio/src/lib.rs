#![forbid(unsafe_code)]
//! CoreAudio device discovery and external device matching.

use std::collections::BTreeSet;

use cpal::traits::{DeviceTrait, HostTrait};
use mars_types::{
    DeviceInventory, ExternalDeviceInfo, MissingExternalPolicy, NodeKind, Profile,
    ResolvedExternalDevice,
};
use regex::Regex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreAudioError {
    #[error("cpal host unavailable: {0}")]
    Host(String),
    #[error("failed to enumerate devices: {0}")]
    Enumerate(String),
}

#[derive(Debug, Default)]
pub struct ExternalResolution {
    pub resolved: Vec<ResolvedExternalDevice>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

pub fn list_device_inventory() -> Result<DeviceInventory, CoreAudioError> {
    let host = cpal::default_host();
    let devices = host
        .devices()
        .map_err(|error| CoreAudioError::Enumerate(error.to_string()))?;

    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    for (index, device) in devices.enumerate() {
        let name = device
            .description()
            .map(|description| description.name().to_string())
            .unwrap_or_else(|_| format!("Unknown Device {index}"));
        let uid = device.id().map_err(|error| {
            CoreAudioError::Enumerate(format!("failed to get stable id for '{name}': {error}"))
        })?;
        let uid = uid.to_string();
        if let Ok(configs) = device.supported_output_configs() {
            let (channels, sample_rates) = collect_output_details(configs);
            outputs.push(ExternalDeviceInfo {
                uid: uid.clone(),
                name: name.clone(),
                manufacturer: None,
                transport: None,
                channels,
                sample_rates,
            });
        }

        if let Ok(configs) = device.supported_input_configs() {
            let (channels, sample_rates) = collect_input_details(configs);
            inputs.push(ExternalDeviceInfo {
                uid,
                name,
                manufacturer: None,
                transport: None,
                channels,
                sample_rates,
            });
        }
    }

    Ok(DeviceInventory { inputs, outputs })
}

/// Best-effort microphone permission probe.
///
/// Returns:
/// - `Some(true)` when input stream config can be queried.
/// - `Some(false)` when host reports authorization/permission denial.
/// - `None` when status is indeterminate (no input device or unknown error).
#[must_use]
pub fn detect_microphone_permission() -> Option<bool> {
    let host = cpal::default_host();
    let device = host.default_input_device()?;
    match device.default_input_config() {
        Ok(_) => Some(true),
        Err(error) => {
            let message = error.to_string().to_lowercase();
            if message.contains("not authorized")
                || message.contains("permission")
                || message.contains("denied")
                || message.contains("tcc")
            {
                Some(false)
            } else {
                None
            }
        }
    }
}

pub fn resolve_externals(profile: &Profile, inventory: &DeviceInventory) -> ExternalResolution {
    let mut resolution = ExternalResolution::default();

    for endpoint in &profile.external.inputs {
        let found = find_match(&endpoint.r#match, &inventory.inputs).or_else(|| {
            endpoint
                .fallback
                .as_ref()
                .and_then(|fallback| find_fallback(fallback, &inventory.inputs))
        });

        match found {
            Some(device) => resolution.resolved.push(ResolvedExternalDevice {
                logical_id: endpoint.id.clone(),
                matched_uid: device.uid.clone(),
                name: device.name.clone(),
                kind: NodeKind::ExternalInput,
                channels: endpoint.channels.unwrap_or(device.channels),
            }),
            None => handle_missing(
                endpoint.id.as_str(),
                endpoint
                    .on_missing
                    .unwrap_or(profile.policy.on_missing_external),
                &mut resolution,
            ),
        }
    }

    for endpoint in &profile.external.outputs {
        let found = find_match(&endpoint.r#match, &inventory.outputs).or_else(|| {
            endpoint
                .fallback
                .as_ref()
                .and_then(|fallback| find_fallback(fallback, &inventory.outputs))
        });

        match found {
            Some(device) => resolution.resolved.push(ResolvedExternalDevice {
                logical_id: endpoint.id.clone(),
                matched_uid: device.uid.clone(),
                name: device.name.clone(),
                kind: NodeKind::ExternalOutput,
                channels: endpoint.channels.unwrap_or(device.channels),
            }),
            None => handle_missing(
                endpoint.id.as_str(),
                endpoint
                    .on_missing
                    .unwrap_or(profile.policy.on_missing_external),
                &mut resolution,
            ),
        }
    }

    resolution
}

fn handle_missing(id: &str, policy: MissingExternalPolicy, resolution: &mut ExternalResolution) {
    let message = format!("external endpoint '{id}' is missing");
    match policy {
        MissingExternalPolicy::Error => resolution.errors.push(message),
        MissingExternalPolicy::Skip => resolution.warnings.push(format!("{message} (skipped)")),
        MissingExternalPolicy::Fallback => resolution
            .errors
            .push(format!("{message} (fallback failed)")),
    }
}

fn find_match<'a>(
    criteria: &mars_types::DeviceMatch,
    candidates: &'a [ExternalDeviceInfo],
) -> Option<&'a ExternalDeviceInfo> {
    let regex = match criteria.name_regex.as_ref() {
        Some(value) => match Regex::new(value) {
            Ok(compiled) => Some(compiled),
            Err(_) => return None,
        },
        None => None,
    };

    candidates.iter().find(|candidate| {
        if let Some(uid) = criteria.uid.as_ref() {
            if candidate.uid != *uid {
                return false;
            }
        }
        if let Some(name) = criteria.name.as_ref() {
            if candidate.name != *name {
                return false;
            }
        }
        if let Some(ref manufacturer) = criteria.manufacturer {
            if candidate.manufacturer.as_ref() != Some(manufacturer) {
                return false;
            }
        }
        if let Some(transport) = criteria.transport {
            if candidate.transport != Some(transport) {
                return false;
            }
        }
        if let Some(ref regex) = regex {
            regex.is_match(&candidate.name)
        } else {
            true
        }
    })
}

fn find_fallback<'a>(
    fallback: &mars_types::FallbackMatch,
    candidates: &'a [ExternalDeviceInfo],
) -> Option<&'a ExternalDeviceInfo> {
    candidates.iter().find(|candidate| {
        if let Some(uid) = fallback.uid.as_ref() {
            return candidate.uid == *uid;
        }
        if let Some(name) = fallback.name.as_ref() {
            return candidate.name == *name;
        }
        false
    })
}

fn collect_output_details(configs: cpal::SupportedOutputConfigs) -> (u16, Vec<u32>) {
    let mut max_channels = 0_u16;
    let mut sample_rates = BTreeSet::<u32>::new();

    for config in configs {
        max_channels = max_channels.max(config.channels());
        sample_rates.insert(config.min_sample_rate());
        sample_rates.insert(config.max_sample_rate());
    }

    (
        if max_channels == 0 { 2 } else { max_channels },
        sample_rates.into_iter().collect(),
    )
}

fn collect_input_details(configs: cpal::SupportedInputConfigs) -> (u16, Vec<u32>) {
    let mut max_channels = 0_u16;
    let mut sample_rates = BTreeSet::<u32>::new();

    for config in configs {
        max_channels = max_channels.max(config.channels());
        sample_rates.insert(config.min_sample_rate());
        sample_rates.insert(config.max_sample_rate());
    }

    (
        if max_channels == 0 { 1 } else { max_channels },
        sample_rates.into_iter().collect(),
    )
}

#[cfg(test)]
mod tests {
    use mars_types::{DeviceMatch, ExternalDeviceInfo};

    use super::find_match;

    fn candidate(name: &str) -> ExternalDeviceInfo {
        ExternalDeviceInfo {
            uid: format!("cpal:input:0:{name}"),
            name: name.to_string(),
            manufacturer: None,
            transport: None,
            channels: 2,
            sample_rates: vec![48_000],
        }
    }

    #[test]
    fn invalid_name_regex_does_not_match_any_candidate() {
        let criteria = DeviceMatch {
            name_regex: Some("*(".to_string()),
            ..DeviceMatch::default()
        };
        let candidates = vec![candidate("Mic One")];

        assert!(find_match(&criteria, &candidates).is_none());
    }

    #[test]
    fn valid_name_regex_matches_candidate() {
        let criteria = DeviceMatch {
            name_regex: Some(".*Mic.*".to_string()),
            ..DeviceMatch::default()
        };
        let candidates = vec![candidate("Mic One")];

        let matched = find_match(&criteria, &candidates);
        assert!(matched.is_some());
        assert_eq!(matched.expect("matched").name, "Mic One");
    }
}

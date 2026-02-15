#![forbid(unsafe_code)]
//! CoreAudio device discovery and external device matching.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use mars_types::{
    DeviceInventory, ExternalDeviceInfo, ExternalRuntimeStatus, NodeKind, Profile,
    ResolvedExternalDevice,
};
use parking_lot::Mutex;
use regex::Regex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreAudioError {
    #[error("cpal host unavailable: {0}")]
    Host(String),
    #[error("failed to enumerate devices: {0}")]
    Enumerate(String),
    #[error("device not found for uid '{uid}'")]
    DeviceNotFound { uid: String },
    #[error("failed to build input stream for uid '{uid}': {reason}")]
    BuildInputStream { uid: String, reason: String },
    #[error("failed to build output stream for uid '{uid}': {reason}")]
    BuildOutputStream { uid: String, reason: String },
    #[error("failed to start stream for uid '{uid}': {reason}")]
    StartStream { uid: String, reason: String },
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
        let found = find_match(&endpoint.r#match, &inventory.inputs);

        match found {
            Some(device) => resolution.resolved.push(ResolvedExternalDevice {
                logical_id: endpoint.id.clone(),
                matched_uid: device.uid.clone(),
                name: device.name.clone(),
                kind: NodeKind::ExternalInput,
                channels: endpoint.channels.unwrap_or(device.channels),
            }),
            None => resolution
                .errors
                .push(format!("external endpoint '{}' is missing", endpoint.id)),
        }
    }

    for endpoint in &profile.external.outputs {
        let found = find_match(&endpoint.r#match, &inventory.outputs);

        match found {
            Some(device) => resolution.resolved.push(ResolvedExternalDevice {
                logical_id: endpoint.id.clone(),
                matched_uid: device.uid.clone(),
                name: device.name.clone(),
                kind: NodeKind::ExternalOutput,
                channels: endpoint.channels.unwrap_or(device.channels),
            }),
            None => resolution
                .errors
                .push(format!("external endpoint '{}' is missing", endpoint.id)),
        }
    }

    resolution
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

const EXTERNAL_QUEUE_PERIODS: usize = 16;

#[derive(Debug, Clone)]
pub struct ExternalEndpointConfig {
    pub node_id: String,
    pub uid: String,
    pub channels: u16,
}

#[derive(Debug, Clone, Default)]
pub struct ExternalRuntimeCounters {
    pub underrun_count: u64,
    pub overrun_count: u64,
    pub xrun_count: u64,
}

#[derive(Debug, Clone)]
pub struct ExternalRuntimeSnapshot {
    pub status: ExternalRuntimeStatus,
    pub counters: ExternalRuntimeCounters,
}

#[derive(Debug)]
struct InputEndpoint {
    node_id: String,
    node_channels: usize,
    device_channels: usize,
    queue: Arc<Mutex<VecDeque<f32>>>,
}

#[derive(Debug)]
struct OutputEndpoint {
    node_id: String,
    node_channels: usize,
    device_channels: usize,
    queue: Arc<Mutex<VecDeque<f32>>>,
    max_samples: usize,
}

pub struct ExternalIoRuntime {
    input_streams: Vec<Stream>,
    output_streams: Vec<Stream>,
    input_endpoints: BTreeMap<String, InputEndpoint>,
    output_endpoints: BTreeMap<String, OutputEndpoint>,
    stream_errors: Arc<Mutex<Vec<String>>>,
    restart_attempts: Arc<AtomicU64>,
    underrun_count: Arc<AtomicU64>,
    overrun_count: Arc<AtomicU64>,
    xrun_count: Arc<AtomicU64>,
}

impl std::fmt::Debug for ExternalIoRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalIoRuntime")
            .field("input_streams", &self.input_streams.len())
            .field("output_streams", &self.output_streams.len())
            .field(
                "input_endpoints",
                &self.input_endpoints.keys().collect::<Vec<_>>(),
            )
            .field(
                "output_endpoints",
                &self.output_endpoints.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ExternalIoRuntime {
    pub fn start(
        sample_rate: u32,
        buffer_frames: u32,
        inputs: &[ExternalEndpointConfig],
        outputs: &[ExternalEndpointConfig],
    ) -> Result<Self, CoreAudioError> {
        let host = cpal::default_host();

        let stream_errors = Arc::new(Mutex::new(Vec::new()));
        let restart_attempts = Arc::new(AtomicU64::new(0));
        let underrun_count = Arc::new(AtomicU64::new(0));
        let overrun_count = Arc::new(AtomicU64::new(0));
        let xrun_count = Arc::new(AtomicU64::new(0));

        let mut input_streams = Vec::new();
        let mut output_streams = Vec::new();
        let mut input_endpoints = BTreeMap::new();
        let mut output_endpoints = BTreeMap::new();

        for endpoint in inputs {
            let device = find_device_by_uid(&host, &endpoint.uid)?;
            let (stream_config, sample_format) =
                select_input_stream_config(&device, endpoint.channels, sample_rate).map_err(
                    |reason| CoreAudioError::BuildInputStream {
                        uid: endpoint.uid.clone(),
                        reason,
                    },
                )?;
            let device_channels = stream_config.channels as usize;
            let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
            let max_samples =
                buffer_frames as usize * EXTERNAL_QUEUE_PERIODS * device_channels.max(1);

            let callback_queue = queue.clone();
            let callback_overrun = overrun_count.clone();
            let callback_errors = stream_errors.clone();
            let callback_xrun = xrun_count.clone();
            let callback_xrun_error = callback_xrun.clone();
            let uid = endpoint.uid.clone();
            let stream = build_input_stream_for_format(
                &device,
                sample_format,
                &stream_config,
                move |samples| {
                    push_samples(
                        &callback_queue,
                        samples,
                        max_samples,
                        &callback_overrun,
                        &callback_xrun,
                    );
                },
                move |error| {
                    callback_xrun_error.fetch_add(1, Ordering::Relaxed);
                    callback_errors
                        .lock()
                        .push(format!("external input stream error for '{uid}': {error}"));
                },
            )
            .map_err(|reason| CoreAudioError::BuildInputStream {
                uid: endpoint.uid.clone(),
                reason,
            })?;
            stream.play().map_err(|error| CoreAudioError::StartStream {
                uid: endpoint.uid.clone(),
                reason: error.to_string(),
            })?;

            input_endpoints.insert(
                endpoint.node_id.clone(),
                InputEndpoint {
                    node_id: endpoint.node_id.clone(),
                    node_channels: endpoint.channels as usize,
                    device_channels,
                    queue,
                },
            );
            input_streams.push(stream);
        }

        for endpoint in outputs {
            let device = find_device_by_uid(&host, &endpoint.uid)?;
            let (stream_config, sample_format) =
                select_output_stream_config(&device, endpoint.channels, sample_rate).map_err(
                    |reason| CoreAudioError::BuildOutputStream {
                        uid: endpoint.uid.clone(),
                        reason,
                    },
                )?;
            let device_channels = stream_config.channels as usize;
            let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
            let max_samples =
                buffer_frames as usize * EXTERNAL_QUEUE_PERIODS * device_channels.max(1);

            let callback_queue = queue.clone();
            let callback_underrun = underrun_count.clone();
            let callback_errors = stream_errors.clone();
            let callback_xrun = xrun_count.clone();
            let callback_xrun_error = callback_xrun.clone();
            let uid = endpoint.uid.clone();
            let stream = build_output_stream_for_format(
                &device,
                sample_format,
                &stream_config,
                move |out_samples| {
                    pop_samples(
                        &callback_queue,
                        out_samples,
                        &callback_underrun,
                        &callback_xrun,
                    );
                },
                move |error| {
                    callback_xrun_error.fetch_add(1, Ordering::Relaxed);
                    callback_errors
                        .lock()
                        .push(format!("external output stream error for '{uid}': {error}"));
                },
            )
            .map_err(|reason| CoreAudioError::BuildOutputStream {
                uid: endpoint.uid.clone(),
                reason,
            })?;
            stream.play().map_err(|error| CoreAudioError::StartStream {
                uid: endpoint.uid.clone(),
                reason: error.to_string(),
            })?;

            output_endpoints.insert(
                endpoint.node_id.clone(),
                OutputEndpoint {
                    node_id: endpoint.node_id.clone(),
                    node_channels: endpoint.channels as usize,
                    device_channels,
                    queue,
                    max_samples,
                },
            );
            output_streams.push(stream);
        }

        Ok(Self {
            input_streams,
            output_streams,
            input_endpoints,
            output_endpoints,
            stream_errors,
            restart_attempts,
            underrun_count,
            overrun_count,
            xrun_count,
        })
    }

    pub fn read_input_into(&self, node_id: &str, out: &mut [f32]) -> bool {
        let Some(endpoint) = self.input_endpoints.get(node_id) else {
            return false;
        };

        let frames = out.len() / endpoint.node_channels.max(1);
        let mut queue = endpoint.queue.lock();
        let mut had_missing = false;

        match (endpoint.device_channels, endpoint.node_channels) {
            (dc, nc) if dc == nc => {
                for sample in out.iter_mut() {
                    if let Some(value) = queue.pop_front() {
                        *sample = value;
                    } else {
                        *sample = 0.0;
                        had_missing = true;
                    }
                }
            }
            (1, 2) => {
                for frame in 0..frames {
                    let value = queue.pop_front().unwrap_or_else(|| {
                        had_missing = true;
                        0.0
                    });
                    out[frame * 2] = value;
                    out[frame * 2 + 1] = value;
                }
            }
            (2, 1) => {
                for sample in out.iter_mut().take(frames) {
                    let left = queue.pop_front().unwrap_or_else(|| {
                        had_missing = true;
                        0.0
                    });
                    let right = queue.pop_front().unwrap_or_else(|| {
                        had_missing = true;
                        0.0
                    });
                    *sample = (left + right) * 0.5;
                }
            }
            _ => {
                out.fill(0.0);
                had_missing = true;
            }
        }

        if had_missing {
            self.underrun_count.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    pub fn write_output_from(&self, node_id: &str, data: &[f32]) -> bool {
        let Some(endpoint) = self.output_endpoints.get(node_id) else {
            return false;
        };

        let frames = data.len() / endpoint.node_channels.max(1);
        let mut queue = endpoint.queue.lock();
        let mut overrun = false;

        let mut push_sample = |sample: f32| {
            queue.push_back(sample);
            while queue.len() > endpoint.max_samples {
                let _ = queue.pop_front();
                overrun = true;
            }
        };

        match (endpoint.node_channels, endpoint.device_channels) {
            (nc, dc) if nc == dc => {
                for sample in data {
                    push_sample(*sample);
                }
            }
            (1, 2) => {
                for sample in data.iter().take(frames) {
                    push_sample(*sample);
                    push_sample(*sample);
                }
            }
            (2, 1) => {
                for frame in 0..frames {
                    let left = data.get(frame * 2).copied().unwrap_or(0.0);
                    let right = data.get(frame * 2 + 1).copied().unwrap_or(0.0);
                    push_sample((left + right) * 0.5);
                }
            }
            _ => {}
        }

        if overrun {
            self.overrun_count.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    #[must_use]
    pub fn snapshot(&self) -> ExternalRuntimeSnapshot {
        let _keep_alive = (self.input_streams.len(), self.output_streams.len());
        ExternalRuntimeSnapshot {
            status: ExternalRuntimeStatus {
                connected_inputs: self
                    .input_endpoints
                    .values()
                    .filter(|endpoint| !endpoint.node_id.is_empty())
                    .count(),
                connected_outputs: self
                    .output_endpoints
                    .values()
                    .filter(|endpoint| !endpoint.node_id.is_empty())
                    .count(),
                restart_attempts: self.restart_attempts.load(Ordering::Relaxed),
                stream_errors: self.stream_errors.lock().clone(),
            },
            counters: ExternalRuntimeCounters {
                underrun_count: self.underrun_count.load(Ordering::Relaxed),
                overrun_count: self.overrun_count.load(Ordering::Relaxed),
                xrun_count: self.xrun_count.load(Ordering::Relaxed),
            },
        }
    }
}

fn sample_format_priority(sample_format: SampleFormat) -> usize {
    match sample_format {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::U16 => 2,
        _ => 3,
    }
}

fn select_input_stream_config(
    device: &cpal::Device,
    channels: u16,
    sample_rate: u32,
) -> Result<(StreamConfig, SampleFormat), String> {
    let mut selected: Option<(StreamConfig, SampleFormat, usize)> = None;
    let requested = sample_rate;

    let configs = device
        .supported_input_configs()
        .map_err(|error| error.to_string())?;
    for config in configs {
        if config.channels() != channels {
            continue;
        }
        if requested < config.min_sample_rate() || requested > config.max_sample_rate() {
            continue;
        }

        let format = config.sample_format();
        let priority = sample_format_priority(format);
        let stream_config = config.with_sample_rate(requested).config();
        match selected.as_ref() {
            Some((_, _, best_priority)) if *best_priority <= priority => {}
            _ => selected = Some((stream_config, format, priority)),
        }
    }

    selected
        .map(|(config, format, _)| (config, format))
        .ok_or_else(|| {
            format!(
                "no supported input config for channels={} sample_rate={sample_rate}",
                channels
            )
        })
}

fn select_output_stream_config(
    device: &cpal::Device,
    channels: u16,
    sample_rate: u32,
) -> Result<(StreamConfig, SampleFormat), String> {
    let mut selected: Option<(StreamConfig, SampleFormat, usize)> = None;
    let requested = sample_rate;

    let configs = device
        .supported_output_configs()
        .map_err(|error| error.to_string())?;
    for config in configs {
        if config.channels() != channels {
            continue;
        }
        if requested < config.min_sample_rate() || requested > config.max_sample_rate() {
            continue;
        }

        let format = config.sample_format();
        let priority = sample_format_priority(format);
        let stream_config = config.with_sample_rate(requested).config();
        match selected.as_ref() {
            Some((_, _, best_priority)) if *best_priority <= priority => {}
            _ => selected = Some((stream_config, format, priority)),
        }
    }

    selected
        .map(|(config, format, _)| (config, format))
        .ok_or_else(|| {
            format!(
                "no supported output config for channels={} sample_rate={sample_rate}",
                channels
            )
        })
}

fn find_device_by_uid(host: &cpal::Host, uid: &str) -> Result<cpal::Device, CoreAudioError> {
    let devices = host
        .devices()
        .map_err(|error| CoreAudioError::Enumerate(error.to_string()))?;
    for device in devices {
        if let Ok(id) = device.id() {
            if id.to_string() == uid {
                return Ok(device);
            }
        }
    }
    Err(CoreAudioError::DeviceNotFound {
        uid: uid.to_string(),
    })
}

fn build_input_stream_for_format(
    device: &cpal::Device,
    format: SampleFormat,
    config: &StreamConfig,
    mut on_samples: impl FnMut(&[f32]) + Send + 'static,
    on_error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<Stream, String> {
    match format {
        SampleFormat::F32 => device
            .build_input_stream(
                config,
                move |data: &[f32], _| on_samples(data),
                on_error,
                None,
            )
            .map_err(|error| error.to_string()),
        SampleFormat::I16 => device
            .build_input_stream(
                config,
                {
                    let mut converted = Vec::<f32>::new();
                    move |data: &[i16], _| {
                        converted.resize(data.len(), 0.0);
                        for (out, sample) in converted.iter_mut().zip(data.iter()) {
                            *out = *sample as f32 / i16::MAX as f32;
                        }
                        on_samples(&converted);
                    }
                },
                on_error,
                None,
            )
            .map_err(|error| error.to_string()),
        SampleFormat::U16 => device
            .build_input_stream(
                config,
                {
                    let mut converted = Vec::<f32>::new();
                    move |data: &[u16], _| {
                        converted.resize(data.len(), 0.0);
                        for (out, sample) in converted.iter_mut().zip(data.iter()) {
                            *out = (*sample as f32 / u16::MAX as f32) * 2.0 - 1.0;
                        }
                        on_samples(&converted);
                    }
                },
                on_error,
                None,
            )
            .map_err(|error| error.to_string()),
        sample_format => Err(format!(
            "unsupported input sample format: {sample_format:?}"
        )),
    }
}

fn build_output_stream_for_format(
    device: &cpal::Device,
    format: SampleFormat,
    config: &StreamConfig,
    mut on_samples: impl FnMut(&mut [f32]) + Send + 'static,
    on_error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<Stream, String> {
    match format {
        SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |data: &mut [f32], _| on_samples(data),
                on_error,
                None,
            )
            .map_err(|error| error.to_string()),
        SampleFormat::I16 => device
            .build_output_stream(
                config,
                {
                    let mut converted = Vec::<f32>::new();
                    move |data: &mut [i16], _| {
                        converted.resize(data.len(), 0.0);
                        on_samples(&mut converted);
                        for (out, sample) in data.iter_mut().zip(converted.iter()) {
                            *out = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        }
                    }
                },
                on_error,
                None,
            )
            .map_err(|error| error.to_string()),
        SampleFormat::U16 => device
            .build_output_stream(
                config,
                {
                    let mut converted = Vec::<f32>::new();
                    move |data: &mut [u16], _| {
                        converted.resize(data.len(), 0.0);
                        on_samples(&mut converted);
                        for (out, sample) in data.iter_mut().zip(converted.iter()) {
                            let normalized = (sample.clamp(-1.0, 1.0) + 1.0) * 0.5;
                            *out = (normalized * u16::MAX as f32) as u16;
                        }
                    }
                },
                on_error,
                None,
            )
            .map_err(|error| error.to_string()),
        sample_format => Err(format!(
            "unsupported output sample format: {sample_format:?}"
        )),
    }
}

fn push_samples(
    queue: &Arc<Mutex<VecDeque<f32>>>,
    samples: &[f32],
    max_samples: usize,
    overrun_count: &Arc<AtomicU64>,
    xrun_count: &Arc<AtomicU64>,
) {
    let mut overrun = false;
    if let Some(mut guard) = queue.try_lock() {
        for sample in samples {
            guard.push_back(*sample);
            while guard.len() > max_samples {
                let _ = guard.pop_front();
                overrun = true;
            }
        }
    } else {
        overrun = true;
    }
    if overrun {
        overrun_count.fetch_add(1, Ordering::Relaxed);
        xrun_count.fetch_add(1, Ordering::Relaxed);
    }
}

fn pop_samples(
    queue: &Arc<Mutex<VecDeque<f32>>>,
    out: &mut [f32],
    underrun_count: &Arc<AtomicU64>,
    xrun_count: &Arc<AtomicU64>,
) {
    let mut underrun = false;
    if let Some(mut guard) = queue.try_lock() {
        for sample in out.iter_mut() {
            if let Some(value) = guard.pop_front() {
                *sample = value;
            } else {
                *sample = 0.0;
                underrun = true;
            }
        }
    } else {
        out.fill(0.0);
        underrun = true;
    }
    if underrun {
        underrun_count.fetch_add(1, Ordering::Relaxed);
        xrun_count.fetch_add(1, Ordering::Relaxed);
    }
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
        assert_eq!(matched.map(|device| device.name.as_str()), Some("Mic One"));
    }
}

#![forbid(unsafe_code)]
//! CoreAudio device discovery and external device matching.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::f32::consts::PI;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use mars_shm::{RingSpec, StreamDirection as RingStreamDirection, global_registry, stream_name};
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
    #[error("no default {direction} device is available")]
    DefaultDeviceUnavailable { direction: &'static str },
    #[error("device not found for uid '{uid}'")]
    DeviceNotFound { uid: String },
    #[error(
        "no supported {direction} stream config for uid '{uid}' at {sample_rate} Hz using preferred channels {preferred:?}"
    )]
    UnsupportedChannelCount {
        uid: String,
        direction: &'static str,
        sample_rate: u32,
        preferred: Vec<u16>,
    },
    #[error("no supported {direction} stream config for uid '{uid}' at {sample_rate} Hz")]
    UnsupportedStreamConfig {
        uid: String,
        direction: &'static str,
        sample_rate: u32,
    },
    #[error("failed to build input stream for uid '{uid}': {reason}")]
    BuildInputStream { uid: String, reason: String },
    #[error("failed to build output stream for uid '{uid}': {reason}")]
    BuildOutputStream { uid: String, reason: String },
    #[error("failed to start stream for uid '{uid}': {reason}")]
    StartStream { uid: String, reason: String },
    #[error("loopback probe timed out after {timeout_ms} ms")]
    ProbeTimeout { timeout_ms: u64 },
    #[error("loopback probe did not find a reliable correlation peak (score={score:.3})")]
    ProbeCorrelationLow { score: f32 },
    #[error("loopback probe failed: {reason}")]
    Probe { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDirection {
    Input,
    Output,
}

impl StreamDirection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopbackProbeRequest {
    pub output_uid: String,
    pub input_uid: String,
    pub sample_rate: u32,
    pub output_channels: u16,
    pub input_channels: u16,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoopbackProbeResult {
    pub latency_frames: i64,
    pub latency_ms: f64,
    pub correlation_score: f32,
    pub output_signal_frames: usize,
    pub captured_frames: usize,
}

#[derive(Debug, Clone)]
pub struct VinRingLoopbackProbeRequest {
    pub output_uid: String,
    pub vin_uid: String,
    pub sample_rate: u32,
    pub output_channels: u16,
    pub vin_channels: u16,
    pub buffer_frames: u32,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct VinRingMonitorRequest {
    pub vin_uid: String,
    pub sample_rate: u32,
    pub vin_channels: u16,
    pub buffer_frames: u32,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VinRingMonitorResult {
    pub captured_frames: usize,
    pub peak: f32,
    pub rms: f32,
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

pub fn default_device_info(
    direction: StreamDirection,
) -> Result<ExternalDeviceInfo, CoreAudioError> {
    let host = cpal::default_host();
    let device = match direction {
        StreamDirection::Input => host.default_input_device(),
        StreamDirection::Output => host.default_output_device(),
    }
    .ok_or(CoreAudioError::DefaultDeviceUnavailable {
        direction: direction.as_str(),
    })?;
    device_info_from_device(&device, direction, 0)
}

pub fn resolve_channel_count(
    uid: &str,
    direction: StreamDirection,
    sample_rate: u32,
    preferred: &[u16],
) -> Result<u16, CoreAudioError> {
    let host = cpal::default_host();
    let device = find_device_by_uid(&host, uid)?;

    for &channels in preferred {
        let supported = match direction {
            StreamDirection::Input => {
                select_input_stream_config(&device, channels, sample_rate).is_ok()
            }
            StreamDirection::Output => {
                select_output_stream_config(&device, channels, sample_rate).is_ok()
            }
        };
        if supported {
            return Ok(channels);
        }
    }

    Err(CoreAudioError::UnsupportedChannelCount {
        uid: uid.to_string(),
        direction: direction.as_str(),
        sample_rate,
        preferred: preferred.to_vec(),
    })
}

pub fn supported_channel_counts(
    uid: &str,
    direction: StreamDirection,
    sample_rate: u32,
) -> Result<Vec<u16>, CoreAudioError> {
    let host = cpal::default_host();
    let device = find_device_by_uid(&host, uid)?;
    let mut counts = BTreeSet::new();

    match direction {
        StreamDirection::Input => {
            let configs = device
                .supported_input_configs()
                .map_err(|error| CoreAudioError::Enumerate(error.to_string()))?;
            for config in configs {
                if sample_rate >= config.min_sample_rate()
                    && sample_rate <= config.max_sample_rate()
                {
                    counts.insert(config.channels());
                }
            }
        }
        StreamDirection::Output => {
            let configs = device
                .supported_output_configs()
                .map_err(|error| CoreAudioError::Enumerate(error.to_string()))?;
            for config in configs {
                if sample_rate >= config.min_sample_rate()
                    && sample_rate <= config.max_sample_rate()
                {
                    counts.insert(config.channels());
                }
            }
        }
    }

    if counts.is_empty() {
        return Err(CoreAudioError::UnsupportedStreamConfig {
            uid: uid.to_string(),
            direction: direction.as_str(),
            sample_rate,
        });
    }

    Ok(counts.into_iter().collect())
}

pub fn measure_loopback_latency(
    request: &LoopbackProbeRequest,
) -> Result<LoopbackProbeResult, CoreAudioError> {
    const STARTUP_SETTLE: Duration = Duration::from_millis(75);
    const MIN_CORRELATION_SCORE: f32 = 0.2;

    if request.sample_rate == 0 {
        return Err(CoreAudioError::Probe {
            reason: "sample_rate must be > 0".to_string(),
        });
    }
    if request.output_channels == 0 || request.input_channels == 0 {
        return Err(CoreAudioError::Probe {
            reason: "input/output channels must be > 0".to_string(),
        });
    }

    let host = cpal::default_host();
    let output_device = find_device_by_uid(&host, &request.output_uid)?;
    let input_device = find_device_by_uid(&host, &request.input_uid)?;
    let (output_config, output_format) =
        select_output_stream_config(&output_device, request.output_channels, request.sample_rate)
            .map_err(|reason| CoreAudioError::BuildOutputStream {
            uid: request.output_uid.clone(),
            reason,
        })?;
    let (input_config, input_format) =
        select_input_stream_config(&input_device, request.input_channels, request.sample_rate)
            .map_err(|reason| CoreAudioError::BuildInputStream {
                uid: request.input_uid.clone(),
                reason,
            })?;

    let pre_roll_frames = ((request.sample_rate as f32) * 0.05) as usize;
    let probe_frames = ((request.sample_rate as f32) * 0.15) as usize;
    let post_roll_frames = ((request.sample_rate as f32) * 1.0) as usize;
    let probe = build_probe_signal(probe_frames.max(512));
    let output_mono = build_output_timeline(&probe, pre_roll_frames, post_roll_frames);
    let output_signal_frames = output_mono.len();
    let output_interleaved = interleave_mono_signal(&output_mono, request.output_channels as usize);
    let target_capture_frames = output_signal_frames + (request.sample_rate as usize / 2);
    let max_capture_samples = target_capture_frames * request.input_channels as usize;

    let playback_state = Arc::new(ProbePlaybackState {
        samples: output_interleaved,
        cursor: AtomicUsize::new(0),
        finished: AtomicBool::new(false),
    });
    let capture = Arc::new(Mutex::new(Vec::<f32>::with_capacity(max_capture_samples)));
    let callback_error = Arc::new(Mutex::new(None::<String>));

    let input_capture = capture.clone();
    let input_errors = callback_error.clone();
    let input_stream = build_input_stream_for_format(
        &input_device,
        input_format,
        &input_config,
        move |samples| {
            let mut guard = input_capture.lock();
            let remaining = max_capture_samples.saturating_sub(guard.len());
            if remaining == 0 {
                return;
            }
            let limit = remaining.min(samples.len());
            guard.extend_from_slice(&samples[..limit]);
        },
        move |error| {
            let mut guard = input_errors.lock();
            *guard = Some(format!("input stream error: {error}"));
        },
    )
    .map_err(|reason| CoreAudioError::BuildInputStream {
        uid: request.input_uid.clone(),
        reason,
    })?;

    let output_state = playback_state.clone();
    let output_errors = callback_error.clone();
    let output_stream = build_output_stream_for_format(
        &output_device,
        output_format,
        &output_config,
        move |out| {
            let cursor = output_state.cursor.load(Ordering::Relaxed);
            let remaining = output_state.samples.len().saturating_sub(cursor);
            let copy_len = remaining.min(out.len());
            if copy_len > 0 {
                out[..copy_len].copy_from_slice(&output_state.samples[cursor..cursor + copy_len]);
                output_state
                    .cursor
                    .store(cursor.saturating_add(copy_len), Ordering::Relaxed);
            }
            if copy_len < out.len() {
                out[copy_len..].fill(0.0);
                output_state.finished.store(true, Ordering::Relaxed);
            } else if cursor.saturating_add(copy_len) >= output_state.samples.len() {
                output_state.finished.store(true, Ordering::Relaxed);
            }
        },
        move |error| {
            let mut guard = output_errors.lock();
            *guard = Some(format!("output stream error: {error}"));
        },
    )
    .map_err(|reason| CoreAudioError::BuildOutputStream {
        uid: request.output_uid.clone(),
        reason,
    })?;

    input_stream
        .play()
        .map_err(|error| CoreAudioError::StartStream {
            uid: request.input_uid.clone(),
            reason: error.to_string(),
        })?;
    std::thread::sleep(STARTUP_SETTLE);
    output_stream
        .play()
        .map_err(|error| CoreAudioError::StartStream {
            uid: request.output_uid.clone(),
            reason: error.to_string(),
        })?;

    let deadline = Instant::now() + request.timeout;
    loop {
        if let Some(reason) = callback_error.lock().clone() {
            return Err(CoreAudioError::Probe { reason });
        }

        let captured_samples = capture.lock().len();
        if playback_state.finished.load(Ordering::Relaxed)
            && captured_samples >= max_capture_samples
        {
            break;
        }

        if Instant::now() >= deadline {
            return Err(CoreAudioError::ProbeTimeout {
                timeout_ms: request.timeout.as_millis() as u64,
            });
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    drop(output_stream);
    drop(input_stream);

    if let Some(reason) = callback_error.lock().clone() {
        return Err(CoreAudioError::Probe { reason });
    }

    let captured = capture.lock().clone();
    let captured_mono = collapse_to_mono(&captured, request.input_channels as usize);
    let (peak_index, correlation_score) = estimate_latency_frames(&captured_mono, &probe)?;
    if correlation_score < MIN_CORRELATION_SCORE {
        return Err(CoreAudioError::ProbeCorrelationLow {
            score: correlation_score,
        });
    }
    let latency_frames = peak_index as i64 - pre_roll_frames as i64;
    let latency_ms = latency_frames as f64 * 1000.0 / request.sample_rate as f64;
    Ok(LoopbackProbeResult {
        latency_frames,
        latency_ms,
        correlation_score,
        output_signal_frames,
        captured_frames: captured_mono.len(),
    })
}

pub fn measure_vin_ring_loopback_latency(
    request: &VinRingLoopbackProbeRequest,
) -> Result<LoopbackProbeResult, CoreAudioError> {
    const STARTUP_SETTLE: Duration = Duration::from_millis(100);
    const MIN_CORRELATION_SCORE: f32 = 0.1;

    if request.sample_rate == 0 || request.output_channels == 0 || request.vin_channels == 0 {
        return Err(CoreAudioError::Probe {
            reason: "sample_rate and channel counts must be > 0".to_string(),
        });
    }
    if request.buffer_frames == 0 {
        return Err(CoreAudioError::Probe {
            reason: "buffer_frames must be > 0".to_string(),
        });
    }

    let probe_frames = ((request.sample_rate as f32) * 0.15) as usize;
    let post_roll_frames = ((request.sample_rate as f32) * 1.0) as usize;
    let probe = build_probe_signal(probe_frames.max(512));
    // The ring-based probe measures stream-frame latency inside MARS, not wall-clock
    // delay before the daemon starts consuming the ring. Using a synthetic pre-roll
    // or subtracting startup settle time biases the estimate negative because those
    // idle periods are not guaranteed to appear in the captured vin stream.
    let output_mono = build_output_timeline(&probe, 0, post_roll_frames);
    let output_signal_frames = output_mono.len();
    let output_interleaved = interleave_mono_signal(&output_mono, request.output_channels as usize);
    let target_capture_frames = output_signal_frames + (request.sample_rate as usize / 2);
    let max_capture_samples = target_capture_frames * request.vin_channels as usize;

    let vout_ring_spec = RingSpec {
        sample_rate: request.sample_rate,
        channels: request.output_channels,
        capacity_frames: request.buffer_frames.saturating_mul(8),
    };
    let vout_ring_name = stream_name(RingStreamDirection::Vout, &request.output_uid);
    let vout_ring = global_registry()
        .create_or_open(&vout_ring_name, vout_ring_spec)
        .map_err(|error| CoreAudioError::Probe {
            reason: format!("failed to open vout ring '{vout_ring_name}': {error}"),
        })?;
    drain_ring(&vout_ring, request.output_channels as usize)?;

    let vin_ring_spec = RingSpec {
        sample_rate: request.sample_rate,
        channels: request.vin_channels,
        capacity_frames: request.buffer_frames.saturating_mul(8),
    };
    let vin_ring_name = stream_name(RingStreamDirection::Vin, &request.vin_uid);
    let vin_ring = global_registry()
        .create_or_open(&vin_ring_name, vin_ring_spec)
        .map_err(|error| CoreAudioError::Probe {
            reason: format!("failed to open vin ring '{vin_ring_name}': {error}"),
        })?;
    drain_ring(&vin_ring, request.vin_channels as usize)?;

    let deadline = Instant::now() + request.timeout;
    let chunk_frames = request.buffer_frames as usize;
    let output_channels = request.output_channels as usize;
    let mut output_cursor = 0usize;
    let mut captured = Vec::<f32>::with_capacity(max_capture_samples);
    std::thread::sleep(STARTUP_SETTLE);
    while Instant::now() < deadline {
        let mut made_progress = false;
        if output_cursor < output_interleaved.len() {
            let writable_frames = {
                let ring = vout_ring.lock();
                let header = ring.header().map_err(|error| CoreAudioError::Probe {
                    reason: format!("failed to inspect vout ring: {error}"),
                })?;
                let used_frames = header.write_idx.saturating_sub(header.read_idx) as usize;
                (header.capacity_frames as usize).saturating_sub(used_frames)
            };
            if writable_frames > 0 {
                let remaining_frames = (output_interleaved.len() - output_cursor) / output_channels;
                let frames_to_write = writable_frames.min(remaining_frames).min(chunk_frames);
                let end = output_cursor + frames_to_write * output_channels;
                let written_frames = vout_ring
                    .lock()
                    .write_interleaved(&output_interleaved[output_cursor..end])
                    .map_err(|error| CoreAudioError::Probe {
                        reason: format!("failed to write vout ring: {error}"),
                    })?;
                if written_frames > 0 {
                    output_cursor = output_cursor
                        .saturating_add(written_frames.saturating_mul(output_channels));
                    made_progress = true;
                }
            }
        }

        let captured_before = captured.len();
        read_ring_samples(
            &vin_ring,
            request.vin_channels as usize,
            max_capture_samples,
            &mut captured,
        )?;
        made_progress |= captured.len() > captured_before;

        if output_cursor >= output_interleaved.len() && captured.len() >= max_capture_samples {
            break;
        }

        if !made_progress {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    if captured.len() < max_capture_samples {
        read_ring_samples(
            &vin_ring,
            request.vin_channels as usize,
            max_capture_samples,
            &mut captured,
        )?;
    }
    if captured.len() < probe.len() * request.vin_channels as usize {
        return Err(CoreAudioError::ProbeTimeout {
            timeout_ms: request.timeout.as_millis() as u64,
        });
    }

    let captured_mono = collapse_to_mono(&captured, request.vin_channels as usize);
    let (peak_index, correlation_score) = estimate_latency_frames(&captured_mono, &probe)?;
    if correlation_score < MIN_CORRELATION_SCORE {
        return Err(CoreAudioError::ProbeCorrelationLow {
            score: correlation_score,
        });
    }
    let latency_frames = peak_index as i64;
    let latency_ms = latency_frames as f64 * 1000.0 / request.sample_rate as f64;
    Ok(LoopbackProbeResult {
        latency_frames,
        latency_ms,
        correlation_score,
        output_signal_frames,
        captured_frames: captured_mono.len(),
    })
}

pub fn monitor_vin_ring_signal(
    request: &VinRingMonitorRequest,
) -> Result<VinRingMonitorResult, CoreAudioError> {
    if request.sample_rate == 0 || request.vin_channels == 0 {
        return Err(CoreAudioError::Probe {
            reason: "sample_rate and vin_channels must be > 0".to_string(),
        });
    }
    if request.buffer_frames == 0 {
        return Err(CoreAudioError::Probe {
            reason: "buffer_frames must be > 0".to_string(),
        });
    }

    let vin_ring_spec = RingSpec {
        sample_rate: request.sample_rate,
        channels: request.vin_channels,
        capacity_frames: request.buffer_frames.saturating_mul(8),
    };
    let vin_ring_name = stream_name(RingStreamDirection::Vin, &request.vin_uid);
    let vin_ring = global_registry()
        .create_or_open(&vin_ring_name, vin_ring_spec)
        .map_err(|error| CoreAudioError::Probe {
            reason: format!("failed to open vin ring '{vin_ring_name}': {error}"),
        })?;
    drain_ring(&vin_ring, request.vin_channels as usize)?;

    let deadline = Instant::now() + request.timeout;
    let mut captured = Vec::<f32>::new();
    let max_capture_samples = request.sample_rate as usize
        * request.timeout.as_secs().max(1) as usize
        * request.vin_channels as usize
        * 2;
    while Instant::now() < deadline {
        read_ring_samples(
            &vin_ring,
            request.vin_channels as usize,
            max_capture_samples,
            &mut captured,
        )?;
        std::thread::sleep(Duration::from_millis(5));
    }
    read_ring_samples(
        &vin_ring,
        request.vin_channels as usize,
        max_capture_samples,
        &mut captured,
    )?;

    let captured_mono = collapse_to_mono(&captured, request.vin_channels as usize);
    let mut peak = 0.0_f32;
    let mut sum_squares = 0.0_f64;
    for sample in &captured_mono {
        peak = peak.max(sample.abs());
        let sample = *sample as f64;
        sum_squares += sample * sample;
    }
    let rms = if captured_mono.is_empty() {
        0.0
    } else {
        (sum_squares / captured_mono.len() as f64).sqrt() as f32
    };
    Ok(VinRingMonitorResult {
        captured_frames: captured_mono.len(),
        peak,
        rms,
    })
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
            Some(result) => {
                if result.used_unknown_metadata {
                    resolution.warnings.push(format!(
                        "external endpoint '{}' matched '{}' using best-effort metadata (manufacturer/transport unavailable)",
                        endpoint.id, result.device.name
                    ));
                }
                resolution.resolved.push(ResolvedExternalDevice {
                    logical_id: endpoint.id.clone(),
                    matched_uid: result.device.uid.clone(),
                    name: result.device.name.clone(),
                    kind: NodeKind::ExternalInput,
                    channels: endpoint.channels.unwrap_or(result.device.channels),
                });
            }
            None => resolution
                .errors
                .push(format!("external endpoint '{}' is missing", endpoint.id)),
        }
    }

    for endpoint in &profile.external.outputs {
        let found = find_match(&endpoint.r#match, &inventory.outputs);

        match found {
            Some(result) => {
                if result.used_unknown_metadata {
                    resolution.warnings.push(format!(
                        "external endpoint '{}' matched '{}' using best-effort metadata (manufacturer/transport unavailable)",
                        endpoint.id, result.device.name
                    ));
                }
                resolution.resolved.push(ResolvedExternalDevice {
                    logical_id: endpoint.id.clone(),
                    matched_uid: result.device.uid.clone(),
                    name: result.device.name.clone(),
                    kind: NodeKind::ExternalOutput,
                    channels: endpoint.channels.unwrap_or(result.device.channels),
                });
            }
            None => resolution
                .errors
                .push(format!("external endpoint '{}' is missing", endpoint.id)),
        }
    }

    resolution
}

#[derive(Debug, Clone, Copy)]
struct MatchResult<'a> {
    device: &'a ExternalDeviceInfo,
    used_unknown_metadata: bool,
}

fn find_match<'a>(
    criteria: &mars_types::DeviceMatch,
    candidates: &'a [ExternalDeviceInfo],
) -> Option<MatchResult<'a>> {
    let regex = match criteria.name_regex.as_ref() {
        Some(value) => match Regex::new(value) {
            Ok(compiled) => Some(compiled),
            Err(_) => return None,
        },
        None => None,
    };
    let metadata_requested = criteria.manufacturer.is_some() || criteria.transport.is_some();
    let mut best: Option<(u8, u8, MatchResult<'_>)> = None;

    for candidate in candidates {
        if let Some(uid) = criteria.uid.as_ref() {
            if candidate.uid != *uid {
                continue;
            }
        }
        if let Some(name) = criteria.name.as_ref() {
            if candidate.name != *name {
                continue;
            }
        }
        if let Some(ref regex) = regex {
            if !regex.is_match(&candidate.name) {
                continue;
            }
        }

        let mut matched_known_metadata = 0_u8;
        let mut used_unknown_metadata = false;
        if let Some(ref manufacturer) = criteria.manufacturer {
            match candidate.manufacturer.as_ref() {
                Some(value) if value == manufacturer => matched_known_metadata += 1,
                Some(_) => continue,
                None => used_unknown_metadata = true,
            }
        }
        if let Some(transport) = criteria.transport {
            match candidate.transport {
                Some(value) if value == transport => matched_known_metadata += 1,
                Some(_) => continue,
                None => used_unknown_metadata = true,
            }
        }

        let known_rank = if !used_unknown_metadata { 1 } else { 0 };
        let result = MatchResult {
            device: candidate,
            used_unknown_metadata: metadata_requested && used_unknown_metadata,
        };
        match &best {
            Some((best_known, best_matched, _))
                if *best_known > known_rank
                    || (*best_known == known_rank && *best_matched >= matched_known_metadata) => {}
            _ => best = Some((known_rank, matched_known_metadata, result)),
        }
    }

    best.map(|(_, _, result)| result)
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

fn device_info_from_device(
    device: &cpal::Device,
    direction: StreamDirection,
    index: usize,
) -> Result<ExternalDeviceInfo, CoreAudioError> {
    let name = device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| format!("Unknown Device {index}"));
    let uid = device.id().map_err(|error| {
        CoreAudioError::Enumerate(format!("failed to get stable id for '{name}': {error}"))
    })?;
    let uid = uid.to_string();
    let (channels, sample_rates) = match direction {
        StreamDirection::Input => {
            let configs = device
                .supported_input_configs()
                .map_err(|error| CoreAudioError::Enumerate(error.to_string()))?;
            collect_input_details(configs)
        }
        StreamDirection::Output => {
            let configs = device
                .supported_output_configs()
                .map_err(|error| CoreAudioError::Enumerate(error.to_string()))?;
            collect_output_details(configs)
        }
    };
    Ok(ExternalDeviceInfo {
        uid,
        name,
        manufacturer: None,
        transport: None,
        channels,
        sample_rates,
    })
}

#[derive(Debug)]
struct ProbePlaybackState {
    samples: Vec<f32>,
    cursor: AtomicUsize,
    finished: AtomicBool,
}

fn build_probe_signal(frames: usize) -> Vec<f32> {
    let start_hz = 450.0_f32;
    let end_hz = 6_500.0_f32;
    let chirp_start = frames.saturating_sub(1).min(64);
    let chirp_frames = frames.saturating_sub(chirp_start).max(1);
    let denom = chirp_frames.saturating_sub(1).max(1) as f32;
    let mut out = vec![0.0_f32; frames];
    if let Some(first) = out.first_mut() {
        *first = 0.95;
    }
    for index in 0..chirp_frames {
        let t = index as f32 / denom;
        let phase = 2.0 * PI * (start_hz * t + ((end_hz - start_hz) * 0.5 * t * t));
        let window = 0.5 - 0.5 * (2.0 * PI * t).cos();
        out[chirp_start + index] += 0.8 * window * phase.sin();
    }
    out
}

fn build_output_timeline(
    probe: &[f32],
    pre_roll_frames: usize,
    post_roll_frames: usize,
) -> Vec<f32> {
    let mut out = Vec::with_capacity(pre_roll_frames + probe.len() + post_roll_frames);
    out.resize(pre_roll_frames, 0.0);
    out.extend_from_slice(probe);
    out.resize(out.len() + post_roll_frames, 0.0);
    out
}

fn interleave_mono_signal(signal: &[f32], channels: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(signal.len() * channels.max(1));
    for &sample in signal {
        for _ in 0..channels.max(1) {
            out.push(sample);
        }
    }
    out
}

fn collapse_to_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }

    let frames = samples.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for frame in 0..frames {
        let mut sum = 0.0_f32;
        for channel in 0..channels {
            sum += samples[frame * channels + channel];
        }
        out.push(sum / channels as f32);
    }
    out
}

fn estimate_latency_frames(
    captured: &[f32],
    probe: &[f32],
) -> Result<(usize, f32), CoreAudioError> {
    if captured.len() < probe.len() {
        return Err(CoreAudioError::Probe {
            reason: format!(
                "captured stream too short for correlation (captured_frames={}, probe_frames={})",
                captured.len(),
                probe.len()
            ),
        });
    }

    let probe_energy = probe
        .iter()
        .map(|sample| {
            let value = *sample as f64;
            value * value
        })
        .sum::<f64>();
    if probe_energy <= f64::EPSILON {
        return Err(CoreAudioError::Probe {
            reason: "probe signal energy is zero".to_string(),
        });
    }

    let mut best_index = 0usize;
    let mut best_score = 0.0_f32;
    for start in 0..=captured.len() - probe.len() {
        let mut dot = 0.0_f64;
        let mut window_energy = 0.0_f64;
        for (sample, probe_sample) in captured[start..start + probe.len()]
            .iter()
            .zip(probe.iter())
        {
            let sample = *sample as f64;
            let probe_sample = *probe_sample as f64;
            dot += sample * probe_sample;
            window_energy += sample * sample;
        }

        if window_energy <= f64::EPSILON {
            continue;
        }

        let score = (dot / (probe_energy.sqrt() * window_energy.sqrt())) as f32;
        let magnitude = score.abs();
        if magnitude > best_score {
            best_score = magnitude;
            best_index = start;
        }
    }

    Ok((best_index, best_score))
}

fn drain_ring(ring: &mars_shm::SharedRingHandle, channels: usize) -> Result<(), CoreAudioError> {
    let mut scratch = vec![0.0_f32; channels.max(1) * 512];
    loop {
        let read_frames = ring
            .lock()
            .read_interleaved(&mut scratch)
            .map_err(|error| CoreAudioError::Probe {
                reason: format!("failed to drain vin ring: {error}"),
            })?;
        if read_frames == 0 {
            return Ok(());
        }
    }
}

fn read_ring_samples(
    ring: &mars_shm::SharedRingHandle,
    channels: usize,
    max_capture_samples: usize,
    captured: &mut Vec<f32>,
) -> Result<(), CoreAudioError> {
    let remaining = max_capture_samples.saturating_sub(captured.len());
    if remaining == 0 {
        return Ok(());
    }

    let frame_capacity = (remaining / channels.max(1)).max(1);
    let mut scratch = vec![0.0_f32; frame_capacity * channels.max(1)];
    let read_frames = ring
        .lock()
        .read_interleaved(&mut scratch)
        .map_err(|error| CoreAudioError::Probe {
            reason: format!("failed to read vin ring: {error}"),
        })?;
    let samples = read_frames.saturating_mul(channels.max(1));
    captured.extend_from_slice(&scratch[..samples.min(remaining)]);
    Ok(())
}

const EXTERNAL_QUEUE_PERIODS: usize = 16;
const STREAM_ERROR_RING_CAPACITY: usize = 64;
const ENDPOINT_ERROR_RING_CAPACITY: usize = 32;
const RECOVERY_EVENT_CAPACITY: usize = 256;
const RECOVERY_BASE_BACKOFF_MS: u64 = 250;
const RECOVERY_MAX_BACKOFF_MS: u64 = 10_000;
const RECOVERY_JITTER_MAX_MS: u64 = 200;
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    pub input_endpoints: Vec<ExternalInputEndpointSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum ExternalEndpointHealth {
    Connected,
    Degraded,
    Reconnecting,
    #[default]
    Stopped,
}


#[derive(Debug, Clone, Default)]
pub struct ExternalInputEndpointSnapshot {
    pub node_id: String,
    pub uid: String,
    pub health: ExternalEndpointHealth,
    pub ingested_frames: u64,
    pub underrun_count: u64,
    pub overrun_count: u64,
    pub xrun_count: u64,
    pub restart_attempts: u64,
    pub error_ring: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointPhase {
    Connected,
    Degraded,
    Reconnecting,
    Stopped,
}

impl EndpointPhase {
    const fn is_connected(self) -> bool {
        matches!(self, Self::Connected)
    }

    const fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded | Self::Reconnecting)
    }

    const fn into_health(self) -> ExternalEndpointHealth {
        match self {
            Self::Connected => ExternalEndpointHealth::Connected,
            Self::Degraded => ExternalEndpointHealth::Degraded,
            Self::Reconnecting => ExternalEndpointHealth::Reconnecting,
            Self::Stopped => ExternalEndpointHealth::Stopped,
        }
    }
}

#[derive(Debug)]
enum RecoveryEvent {
    StreamError { kind: EndpointKind, node_id: String },
    StopRequested,
}

struct InputEndpointRuntime {
    device_channels: usize,
    phase: EndpointPhase,
    attempts: u32,
    next_retry_at: Option<Instant>,
    stream: Option<Stream>,
}

#[derive(Debug, Default)]
struct InputEndpointCounters {
    ingested_frames: AtomicU64,
    underrun_count: AtomicU64,
    overrun_count: AtomicU64,
    xrun_count: AtomicU64,
    restart_attempts: AtomicU64,
}

struct OutputEndpointRuntime {
    device_channels: usize,
    max_samples: usize,
    phase: EndpointPhase,
    attempts: u32,
    next_retry_at: Option<Instant>,
    stream: Option<Stream>,
}

#[derive(Clone)]
struct InputEndpoint {
    node_id: String,
    uid: String,
    node_channels: usize,
    sample_rate: u32,
    buffer_frames: u32,
    queue: Arc<Mutex<VecDeque<f32>>>,
    runtime: Arc<Mutex<InputEndpointRuntime>>,
    counters: Arc<InputEndpointCounters>,
    errors: Arc<Mutex<VecDeque<String>>>,
}

#[derive(Clone)]
struct OutputEndpoint {
    node_id: String,
    uid: String,
    node_channels: usize,
    sample_rate: u32,
    buffer_frames: u32,
    queue: Arc<Mutex<VecDeque<f32>>>,
    runtime: Arc<Mutex<OutputEndpointRuntime>>,
}

struct RecoveryContext {
    recovery_tx: SyncSender<RecoveryEvent>,
    recovery_stop: Arc<std::sync::atomic::AtomicBool>,
    stream_errors: Arc<Mutex<VecDeque<String>>>,
    restart_attempts: Arc<AtomicU64>,
    underrun_count: Arc<AtomicU64>,
    overrun_count: Arc<AtomicU64>,
    xrun_count: Arc<AtomicU64>,
    input_endpoints: Vec<InputEndpoint>,
    output_endpoints: Vec<OutputEndpoint>,
}

pub struct ExternalIoRuntime {
    input_endpoints: BTreeMap<String, InputEndpoint>,
    output_endpoints: BTreeMap<String, OutputEndpoint>,
    stream_errors: Arc<Mutex<VecDeque<String>>>,
    restart_attempts: Arc<AtomicU64>,
    underrun_count: Arc<AtomicU64>,
    overrun_count: Arc<AtomicU64>,
    xrun_count: Arc<AtomicU64>,
    recovery_tx: SyncSender<RecoveryEvent>,
    recovery_stop: Arc<std::sync::atomic::AtomicBool>,
    recovery_handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ExternalIoRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalIoRuntime")
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

impl Drop for ExternalIoRuntime {
    fn drop(&mut self) {
        self.recovery_stop.store(true, Ordering::Relaxed);
        let _ = self.recovery_tx.try_send(RecoveryEvent::StopRequested);
        if let Some(handle) = self.recovery_handle.take() {
            let _ = handle.join();
        }

        for endpoint in self.input_endpoints.values() {
            let mut runtime = endpoint.runtime.lock();
            runtime.phase = EndpointPhase::Stopped;
            runtime.stream = None;
        }
        for endpoint in self.output_endpoints.values() {
            let mut runtime = endpoint.runtime.lock();
            runtime.phase = EndpointPhase::Stopped;
            runtime.stream = None;
        }
    }
}

impl ExternalIoRuntime {
    pub fn start(
        sample_rate: u32,
        buffer_frames: u32,
        inputs: &[ExternalEndpointConfig],
        outputs: &[ExternalEndpointConfig],
    ) -> Result<Self, CoreAudioError> {
        let stream_errors = Arc::new(Mutex::new(VecDeque::new()));
        let restart_attempts = Arc::new(AtomicU64::new(0));
        let underrun_count = Arc::new(AtomicU64::new(0));
        let overrun_count = Arc::new(AtomicU64::new(0));
        let xrun_count = Arc::new(AtomicU64::new(0));
        let recovery_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (recovery_tx, recovery_rx) = sync_channel::<RecoveryEvent>(RECOVERY_EVENT_CAPACITY);

        let mut input_endpoints = BTreeMap::new();
        let mut output_endpoints = BTreeMap::new();

        for endpoint in inputs {
            let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
            let runtime = Arc::new(Mutex::new(InputEndpointRuntime {
                device_channels: endpoint.channels as usize,
                phase: EndpointPhase::Degraded,
                attempts: 0,
                next_retry_at: None,
                stream: None,
            }));
            let counters = Arc::new(InputEndpointCounters::default());
            let errors = Arc::new(Mutex::new(VecDeque::new()));
            let input_endpoint = InputEndpoint {
                node_id: endpoint.node_id.clone(),
                uid: endpoint.uid.clone(),
                node_channels: endpoint.channels as usize,
                sample_rate,
                buffer_frames,
                queue,
                runtime,
                counters,
                errors,
            };
            connect_input_endpoint(
                &input_endpoint,
                &recovery_tx,
                &stream_errors,
                &overrun_count,
                &xrun_count,
            )?;
            input_endpoints.insert(input_endpoint.node_id.clone(), input_endpoint);
        }

        for endpoint in outputs {
            let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
            let runtime = Arc::new(Mutex::new(OutputEndpointRuntime {
                device_channels: endpoint.channels as usize,
                max_samples: buffer_frames as usize
                    * EXTERNAL_QUEUE_PERIODS
                    * usize::from(endpoint.channels).max(1),
                phase: EndpointPhase::Degraded,
                attempts: 0,
                next_retry_at: None,
                stream: None,
            }));
            let output_endpoint = OutputEndpoint {
                node_id: endpoint.node_id.clone(),
                uid: endpoint.uid.clone(),
                node_channels: endpoint.channels as usize,
                sample_rate,
                buffer_frames,
                queue,
                runtime,
            };
            connect_output_endpoint(
                &output_endpoint,
                &recovery_tx,
                &stream_errors,
                &underrun_count,
                &xrun_count,
            )?;
            output_endpoints.insert(output_endpoint.node_id.clone(), output_endpoint);
        }

        let recovery_handle = {
            let recovery_context = RecoveryContext {
                recovery_tx: recovery_tx.clone(),
                recovery_stop: recovery_stop.clone(),
                stream_errors: stream_errors.clone(),
                restart_attempts: restart_attempts.clone(),
                underrun_count: underrun_count.clone(),
                overrun_count: overrun_count.clone(),
                xrun_count: xrun_count.clone(),
                input_endpoints: input_endpoints.values().cloned().collect::<Vec<_>>(),
                output_endpoints: output_endpoints.values().cloned().collect::<Vec<_>>(),
            };
            std::thread::Builder::new()
                .name("mars-external-recovery".to_string())
                .spawn(move || {
                    run_recovery_supervisor(recovery_rx, recovery_context);
                })
                .map_err(|error| {
                    CoreAudioError::Host(format!("failed to spawn recovery supervisor: {error}"))
                })?
        };

        Ok(Self {
            input_endpoints,
            output_endpoints,
            stream_errors,
            restart_attempts,
            underrun_count,
            overrun_count,
            xrun_count,
            recovery_tx,
            recovery_stop,
            recovery_handle: Some(recovery_handle),
        })
    }

    pub fn read_input_into(&self, node_id: &str, out: &mut [f32]) -> bool {
        let Some(endpoint) = self.input_endpoints.get(node_id) else {
            return false;
        };

        let (device_channels, phase) = {
            let runtime = endpoint.runtime.lock();
            (runtime.device_channels, runtime.phase)
        };
        if !phase.is_connected() {
            out.fill(0.0);
            self.underrun_count.fetch_add(1, Ordering::Relaxed);
            self.xrun_count.fetch_add(1, Ordering::Relaxed);
            endpoint
                .counters
                .underrun_count
                .fetch_add(1, Ordering::Relaxed);
            endpoint.counters.xrun_count.fetch_add(1, Ordering::Relaxed);
            return true;
        }

        let frames = out.len() / endpoint.node_channels.max(1);
        let mut queue = endpoint.queue.lock();
        let mut had_missing = false;
        let mut ingested_frames = 0_u64;

        match (device_channels, endpoint.node_channels) {
            (dc, nc) if dc == nc => {
                for sample in out.iter_mut() {
                    if let Some(value) = queue.pop_front() {
                        *sample = value;
                        ingested_frames = ingested_frames.saturating_add(1);
                    } else {
                        *sample = 0.0;
                        had_missing = true;
                    }
                }
                ingested_frames /= endpoint.node_channels.max(1) as u64;
            }
            (1, 2) => {
                for frame in 0..frames {
                    let value = if let Some(value) = queue.pop_front() {
                        ingested_frames = ingested_frames.saturating_add(1);
                        value
                    } else {
                        had_missing = true;
                        0.0
                    };
                    out[frame * 2] = value;
                    out[frame * 2 + 1] = value;
                }
            }
            (2, 1) => {
                for sample in out.iter_mut().take(frames) {
                    let left = if let Some(value) = queue.pop_front() {
                        ingested_frames = ingested_frames.saturating_add(1);
                        value
                    } else {
                        had_missing = true;
                        0.0
                    };
                    let right = if let Some(value) = queue.pop_front() {
                        ingested_frames = ingested_frames.saturating_add(1);
                        value
                    } else {
                        had_missing = true;
                        0.0
                    };
                    *sample = (left + right) * 0.5;
                }
                ingested_frames /= 2;
            }
            _ => {
                out.fill(0.0);
                had_missing = true;
            }
        }

        endpoint
            .counters
            .ingested_frames
            .fetch_add(ingested_frames, Ordering::Relaxed);
        if had_missing {
            self.underrun_count.fetch_add(1, Ordering::Relaxed);
            self.xrun_count.fetch_add(1, Ordering::Relaxed);
            endpoint
                .counters
                .underrun_count
                .fetch_add(1, Ordering::Relaxed);
            endpoint.counters.xrun_count.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    pub fn write_output_from(&self, node_id: &str, data: &[f32]) -> bool {
        let Some(endpoint) = self.output_endpoints.get(node_id) else {
            return false;
        };

        let (device_channels, max_samples, phase) = {
            let runtime = endpoint.runtime.lock();
            (runtime.device_channels, runtime.max_samples, runtime.phase)
        };
        if !phase.is_connected() {
            self.overrun_count.fetch_add(1, Ordering::Relaxed);
            self.xrun_count.fetch_add(1, Ordering::Relaxed);
            return true;
        }

        let frames = data.len() / endpoint.node_channels.max(1);
        let mut queue = endpoint.queue.lock();
        let mut overrun = false;

        let mut push_sample = |sample: f32| {
            queue.push_back(sample);
            while queue.len() > max_samples {
                let _ = queue.pop_front();
                overrun = true;
            }
        };

        match (endpoint.node_channels, device_channels) {
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
            _ => overrun = true,
        }

        if overrun {
            self.overrun_count.fetch_add(1, Ordering::Relaxed);
            self.xrun_count.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    #[must_use]
    pub fn snapshot(&self) -> ExternalRuntimeSnapshot {
        let mut connected_inputs = 0usize;
        let mut degraded_inputs = 0usize;
        let mut input_endpoints = Vec::with_capacity(self.input_endpoints.len());
        for endpoint in self.input_endpoints.values() {
            let phase = endpoint.runtime.lock().phase;
            if phase.is_connected() {
                connected_inputs = connected_inputs.saturating_add(1);
            } else if phase.is_degraded() {
                degraded_inputs = degraded_inputs.saturating_add(1);
            }
            input_endpoints.push(ExternalInputEndpointSnapshot {
                node_id: endpoint.node_id.clone(),
                uid: endpoint.uid.clone(),
                health: phase.into_health(),
                ingested_frames: endpoint.counters.ingested_frames.load(Ordering::Relaxed),
                underrun_count: endpoint.counters.underrun_count.load(Ordering::Relaxed),
                overrun_count: endpoint.counters.overrun_count.load(Ordering::Relaxed),
                xrun_count: endpoint.counters.xrun_count.load(Ordering::Relaxed),
                restart_attempts: endpoint.counters.restart_attempts.load(Ordering::Relaxed),
                error_ring: endpoint.errors.lock().iter().cloned().collect(),
            });
        }

        let mut connected_outputs = 0usize;
        let mut degraded_outputs = 0usize;
        for endpoint in self.output_endpoints.values() {
            let phase = endpoint.runtime.lock().phase;
            if phase.is_connected() {
                connected_outputs = connected_outputs.saturating_add(1);
            } else if phase.is_degraded() {
                degraded_outputs = degraded_outputs.saturating_add(1);
            }
        }

        ExternalRuntimeSnapshot {
            status: ExternalRuntimeStatus {
                connected_inputs,
                connected_outputs,
                degraded_inputs,
                degraded_outputs,
                restart_attempts: self.restart_attempts.load(Ordering::Relaxed),
                stream_errors: self.stream_errors.lock().iter().cloned().collect(),
            },
            counters: ExternalRuntimeCounters {
                underrun_count: self.underrun_count.load(Ordering::Relaxed),
                overrun_count: self.overrun_count.load(Ordering::Relaxed),
                xrun_count: self.xrun_count.load(Ordering::Relaxed),
            },
            input_endpoints,
        }
    }
}

fn connect_input_endpoint(
    endpoint: &InputEndpoint,
    recovery_tx: &SyncSender<RecoveryEvent>,
    stream_errors: &Arc<Mutex<VecDeque<String>>>,
    overrun_count: &Arc<AtomicU64>,
    xrun_count: &Arc<AtomicU64>,
) -> Result<(), CoreAudioError> {
    let host = cpal::default_host();
    let device = find_device_by_uid(&host, &endpoint.uid)?;
    let (stream_config, sample_format) =
        select_input_stream_config(&device, endpoint.node_channels as u16, endpoint.sample_rate)
            .map_err(|reason| CoreAudioError::BuildInputStream {
                uid: endpoint.uid.clone(),
                reason,
            })?;
    let device_channels = stream_config.channels as usize;
    let max_samples =
        endpoint.buffer_frames as usize * EXTERNAL_QUEUE_PERIODS * device_channels.max(1);

    let callback_queue = endpoint.queue.clone();
    let callback_overrun = overrun_count.clone();
    let callback_errors = stream_errors.clone();
    let callback_xrun = xrun_count.clone();
    let callback_xrun_error = callback_xrun.clone();
    let callback_events = recovery_tx.clone();
    let callback_endpoint_counters = endpoint.counters.clone();
    let callback_endpoint_counters_error = endpoint.counters.clone();
    let callback_endpoint_errors = endpoint.errors.clone();
    let uid = endpoint.uid.clone();
    let node_id = endpoint.node_id.clone();
    let stream = build_input_stream_for_format(
        &device,
        sample_format,
        &stream_config,
        move |samples| {
            let overrun = push_samples(
                &callback_queue,
                samples,
                max_samples,
                &callback_overrun,
                &callback_xrun,
            );
            if overrun {
                callback_endpoint_counters
                    .overrun_count
                    .fetch_add(1, Ordering::Relaxed);
                callback_endpoint_counters
                    .xrun_count
                    .fetch_add(1, Ordering::Relaxed);
            }
        },
        move |error| {
            callback_xrun_error.fetch_add(1, Ordering::Relaxed);
            let message = format!("external input stream error for '{uid}': {error}");
            push_stream_error(&callback_errors, message.clone());
            push_bounded_error(&callback_endpoint_errors, message);
            callback_endpoint_counters_error
                .xrun_count
                .fetch_add(1, Ordering::Relaxed);
            let _ = callback_events.try_send(RecoveryEvent::StreamError {
                kind: EndpointKind::Input,
                node_id: node_id.clone(),
            });
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

    let mut runtime = endpoint.runtime.lock();
    runtime.device_channels = device_channels;
    runtime.phase = EndpointPhase::Connected;
    runtime.attempts = 0;
    runtime.next_retry_at = None;
    runtime.stream = Some(stream);
    Ok(())
}

fn connect_output_endpoint(
    endpoint: &OutputEndpoint,
    recovery_tx: &SyncSender<RecoveryEvent>,
    stream_errors: &Arc<Mutex<VecDeque<String>>>,
    underrun_count: &Arc<AtomicU64>,
    xrun_count: &Arc<AtomicU64>,
) -> Result<(), CoreAudioError> {
    let host = cpal::default_host();
    let device = find_device_by_uid(&host, &endpoint.uid)?;
    let (stream_config, sample_format) =
        select_output_stream_config(&device, endpoint.node_channels as u16, endpoint.sample_rate)
            .map_err(|reason| CoreAudioError::BuildOutputStream {
            uid: endpoint.uid.clone(),
            reason,
        })?;
    let device_channels = stream_config.channels as usize;
    let max_samples =
        endpoint.buffer_frames as usize * EXTERNAL_QUEUE_PERIODS * device_channels.max(1);

    let callback_queue = endpoint.queue.clone();
    let callback_underrun = underrun_count.clone();
    let callback_errors = stream_errors.clone();
    let callback_xrun = xrun_count.clone();
    let callback_xrun_error = callback_xrun.clone();
    let callback_events = recovery_tx.clone();
    let uid = endpoint.uid.clone();
    let node_id = endpoint.node_id.clone();
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
            let message = format!("external output stream error for '{uid}': {error}");
            push_stream_error(&callback_errors, message);
            let _ = callback_events.try_send(RecoveryEvent::StreamError {
                kind: EndpointKind::Output,
                node_id: node_id.clone(),
            });
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

    let mut runtime = endpoint.runtime.lock();
    runtime.device_channels = device_channels;
    runtime.max_samples = max_samples;
    runtime.phase = EndpointPhase::Connected;
    runtime.attempts = 0;
    runtime.next_retry_at = None;
    runtime.stream = Some(stream);
    Ok(())
}

fn run_recovery_supervisor(recovery_rx: Receiver<RecoveryEvent>, context: RecoveryContext) {
    while !context.recovery_stop.load(Ordering::Relaxed) {
        match recovery_rx.recv_timeout(RECOVERY_POLL_INTERVAL) {
            Ok(RecoveryEvent::StopRequested) => break,
            Ok(RecoveryEvent::StreamError { kind, node_id }) => match kind {
                EndpointKind::Input => {
                    if let Some(endpoint) = context
                        .input_endpoints
                        .iter()
                        .find(|endpoint| endpoint.node_id == node_id)
                    {
                        let mut runtime = endpoint.runtime.lock();
                        mark_endpoint_degraded(&mut runtime, None);
                    }
                }
                EndpointKind::Output => {
                    if let Some(endpoint) = context
                        .output_endpoints
                        .iter()
                        .find(|endpoint| endpoint.node_id == node_id)
                    {
                        let mut runtime = endpoint.runtime.lock();
                        mark_output_endpoint_degraded(&mut runtime, None);
                    }
                }
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let now = Instant::now();

        for endpoint in &context.input_endpoints {
            let should_retry = {
                let mut runtime = endpoint.runtime.lock();
                if runtime.phase == EndpointPhase::Degraded
                    && runtime.next_retry_at.is_some_and(|at| at <= now)
                {
                    runtime.phase = EndpointPhase::Reconnecting;
                    true
                } else {
                    false
                }
            };
            if !should_retry {
                continue;
            }

            context.restart_attempts.fetch_add(1, Ordering::Relaxed);
            endpoint
                .counters
                .restart_attempts
                .fetch_add(1, Ordering::Relaxed);
            if let Err(error) = connect_input_endpoint(
                endpoint,
                &context.recovery_tx,
                &context.stream_errors,
                &context.overrun_count,
                &context.xrun_count,
            ) {
                push_stream_error(
                    &context.stream_errors,
                    format!(
                        "external input reconnect failed for '{}': {error}",
                        endpoint.uid
                    ),
                );
                push_bounded_error(
                    &endpoint.errors,
                    format!("reconnect failed for '{}': {error}", endpoint.uid),
                );
                endpoint.counters.xrun_count.fetch_add(1, Ordering::Relaxed);
                let mut runtime = endpoint.runtime.lock();
                let attempts = runtime.attempts.saturating_add(1);
                mark_endpoint_degraded(&mut runtime, Some(attempts));
            }
        }

        for endpoint in &context.output_endpoints {
            let should_retry = {
                let mut runtime = endpoint.runtime.lock();
                if runtime.phase == EndpointPhase::Degraded
                    && runtime.next_retry_at.is_some_and(|at| at <= now)
                {
                    runtime.phase = EndpointPhase::Reconnecting;
                    true
                } else {
                    false
                }
            };
            if !should_retry {
                continue;
            }

            context.restart_attempts.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = connect_output_endpoint(
                endpoint,
                &context.recovery_tx,
                &context.stream_errors,
                &context.underrun_count,
                &context.xrun_count,
            ) {
                push_stream_error(
                    &context.stream_errors,
                    format!(
                        "external output reconnect failed for '{}': {error}",
                        endpoint.uid
                    ),
                );
                let mut runtime = endpoint.runtime.lock();
                let attempts = runtime.attempts.saturating_add(1);
                mark_output_endpoint_degraded(&mut runtime, Some(attempts));
            }
        }
    }
}

fn mark_endpoint_degraded(runtime: &mut InputEndpointRuntime, attempts: Option<u32>) {
    runtime.phase = EndpointPhase::Degraded;
    runtime.stream = None;
    runtime.attempts = attempts
        .unwrap_or_else(|| runtime.attempts.saturating_add(1))
        .max(1);
    runtime.next_retry_at = Some(Instant::now() + reconnect_backoff(runtime.attempts));
}

fn mark_output_endpoint_degraded(runtime: &mut OutputEndpointRuntime, attempts: Option<u32>) {
    runtime.phase = EndpointPhase::Degraded;
    runtime.stream = None;
    runtime.attempts = attempts
        .unwrap_or_else(|| runtime.attempts.saturating_add(1))
        .max(1);
    runtime.next_retry_at = Some(Instant::now() + reconnect_backoff(runtime.attempts));
}

fn reconnect_backoff(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(20);
    let exponential = RECOVERY_BASE_BACKOFF_MS.saturating_mul(1_u64 << shift);
    let base = exponential.min(RECOVERY_MAX_BACKOFF_MS);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64 % (RECOVERY_JITTER_MAX_MS + 1))
        .unwrap_or(0);
    Duration::from_millis(base.saturating_add(jitter))
}

fn push_stream_error(stream_errors: &Arc<Mutex<VecDeque<String>>>, message: String) {
    push_bounded_error_with_capacity(stream_errors, message, STREAM_ERROR_RING_CAPACITY);
}

fn push_bounded_error(errors: &Arc<Mutex<VecDeque<String>>>, message: String) {
    push_bounded_error_with_capacity(errors, message, ENDPOINT_ERROR_RING_CAPACITY);
}

fn push_bounded_error_with_capacity(
    errors: &Arc<Mutex<VecDeque<String>>>,
    message: String,
    capacity: usize,
) {
    let mut guard = errors.lock();
    if guard.len() >= capacity {
        let _ = guard.pop_front();
    }
    guard.push_back(message);
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
) -> bool {
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
    overrun
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
#[allow(clippy::expect_used)]
mod tests {
    use mars_types::{DeviceMatch, ExternalDeviceInfo, TransportType};

    use super::{build_probe_signal, estimate_latency_frames, find_match};

    fn candidate(
        uid: &str,
        name: &str,
        manufacturer: Option<&str>,
        transport: Option<TransportType>,
    ) -> ExternalDeviceInfo {
        ExternalDeviceInfo {
            uid: uid.to_string(),
            name: name.to_string(),
            manufacturer: manufacturer.map(ToOwned::to_owned),
            transport,
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
        let candidates = vec![candidate("cpal:input:0:mic-one", "Mic One", None, None)];

        assert!(find_match(&criteria, &candidates).is_none());
    }

    #[test]
    fn valid_name_regex_matches_candidate() {
        let criteria = DeviceMatch {
            name_regex: Some(".*Mic.*".to_string()),
            ..DeviceMatch::default()
        };
        let candidates = vec![candidate("cpal:input:0:mic-one", "Mic One", None, None)];

        let matched = find_match(&criteria, &candidates);
        assert!(matched.is_some());
        assert_eq!(
            matched.map(|result| result.device.name.as_str()),
            Some("Mic One")
        );
        assert_eq!(
            matched.map(|result| result.used_unknown_metadata),
            Some(false)
        );
    }

    #[test]
    fn metadata_unknown_allows_best_effort_match() {
        let criteria = DeviceMatch {
            name: Some("Mic One".to_string()),
            manufacturer: Some("Acme".to_string()),
            ..DeviceMatch::default()
        };
        let candidates = vec![candidate("cpal:input:0:mic-one", "Mic One", None, None)];

        let matched = find_match(&criteria, &candidates).expect("match expected");
        assert_eq!(matched.device.uid, "cpal:input:0:mic-one");
        assert!(matched.used_unknown_metadata);
    }

    #[test]
    fn metadata_known_mismatch_rejects_candidate() {
        let criteria = DeviceMatch {
            name: Some("Mic One".to_string()),
            manufacturer: Some("Acme".to_string()),
            ..DeviceMatch::default()
        };
        let candidates = vec![candidate(
            "cpal:input:0:mic-one",
            "Mic One",
            Some("Other"),
            None,
        )];

        assert!(find_match(&criteria, &candidates).is_none());
    }

    #[test]
    fn known_metadata_match_is_preferred_over_unknown() {
        let criteria = DeviceMatch {
            name: Some("Mic One".to_string()),
            manufacturer: Some("Acme".to_string()),
            transport: Some(TransportType::Usb),
            ..DeviceMatch::default()
        };
        let candidates = vec![
            candidate("unknown", "Mic One", None, None),
            candidate("known", "Mic One", Some("Acme"), Some(TransportType::Usb)),
        ];

        let matched = find_match(&criteria, &candidates).expect("match expected");
        assert_eq!(matched.device.uid, "known");
        assert!(!matched.used_unknown_metadata);
    }

    #[test]
    fn latency_estimator_finds_expected_offset() {
        let probe = build_probe_signal(512);
        let mut captured = vec![0.0_f32; 321];
        captured.extend_from_slice(&probe);
        captured.resize(captured.len() + 200, 0.0);

        let (offset, score) = estimate_latency_frames(&captured, &probe).expect("estimate");
        assert_eq!(offset, 321);
        assert!(score > 0.99, "score={score}");
    }

    #[test]
    fn latency_estimator_handles_polarity_inversion() {
        let probe = build_probe_signal(512);
        let mut captured = vec![0.0_f32; 177];
        captured.extend(probe.iter().map(|sample| -*sample));
        captured.resize(captured.len() + 120, 0.0);

        let (offset, score) = estimate_latency_frames(&captured, &probe).expect("estimate");
        assert_eq!(offset, 177);
        assert!(score > 0.99, "score={score}");
    }
}

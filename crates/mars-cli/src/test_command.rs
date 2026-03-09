use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use mars_coreaudio::{
    LoopbackProbeResult, StreamDirection, VinRingLoopbackProbeRequest, VinRingMonitorRequest,
    default_device_info, list_device_inventory, measure_vin_ring_loopback_latency,
    monitor_vin_ring_signal, resolve_channel_count, supported_channel_counts,
};
use mars_ipc::{DaemonRequest, DaemonResponse, IpcClient};
use mars_types::{
    ApplyRequest, AudioConfig, AutoOrU16, AutoOrU32, ClearRequest, ExitCode, ExternalDeviceInfo,
    ExternalDevices, ExternalInput, ExternalOutput, Pipe, Policy, Profile, VirtualDevices,
    VirtualInputDevice, VirtualOutputDevice,
};
use serde::Serialize;
use tokio::time::sleep;

use crate::{CliError, ipc_to_cli_error};

const TEST_VIRTUAL_OUTPUT_ID: &str = "test-system";
const TEST_VIRTUAL_OUTPUT_NAME: &str = "Bus: Test System";
const TEST_VIRTUAL_OUTPUT_UID: &str = "com.mars.tso";
const TEST_VIRTUAL_INPUT_ID: &str = "test-capture";
const TEST_VIRTUAL_INPUT_NAME: &str = "Mix: Test Capture";
const TEST_VIRTUAL_INPUT_UID: &str = "com.mars.tsi";
const TEST_MIC_ID: &str = "test-mic";
const TEST_SPEAKER_ID: &str = "test-speaker";
const BUILTIN_MIC_UID: &str = "coreaudio:BuiltInMicrophoneDevice";
const BUILTIN_SPEAKER_UID: &str = "coreaudio:BuiltInSpeakerDevice";
const TEST_APPLY_TIMEOUT_MS: u64 = 5_000;
const TEST_PROBE_TIMEOUT: Duration = Duration::from_secs(4);
pub(crate) const ROUTE_LISTEN_DURATION: Duration = Duration::from_secs(5);
const TEST_DEVICE_READY_INTERVAL: Duration = Duration::from_millis(100);
const COREAUDIO_UID_PREFIX: &str = "coreaudio:";
const TEST_SIGNAL_PEAK_THRESHOLD: f32 = 0.008;

#[derive(Debug, Serialize)]
struct ProbeReport {
    latency_frames: i64,
    latency_ms: f64,
    correlation_score: f32,
    output_signal_frames: usize,
    captured_frames: usize,
}

#[derive(Debug, Serialize)]
struct TestEndpointReport {
    kind: String,
    logical_id: Option<String>,
    name: String,
    uid: String,
    channels: u16,
}

#[derive(Debug, Serialize)]
struct TestContextReport {
    mode: String,
    description: String,
    signal: String,
    source: TestEndpointReport,
    sinks: Vec<TestEndpointReport>,
    paths: Vec<String>,
    includes: Vec<String>,
    excludes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct InternalLatencyTestReport {
    context: TestContextReport,
    sample_rate: u32,
    buffer_frames: u32,
    virtual_system_output_uid: String,
    virtual_capture_input_uid: String,
    internal: ProbeReport,
    notes: Vec<String>,
    restored_profile: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RouteTestReport {
    context: TestContextReport,
    sample_rate: u32,
    buffer_frames: u32,
    listen_ms: u64,
    microphone: ExternalDeviceInfo,
    speaker: ExternalDeviceInfo,
    virtual_capture_input_uid: String,
    external_input_connected: bool,
    external_output_connected: bool,
    capture_signal_detected: bool,
    peak_dbfs: Option<f32>,
    rms_dbfs: Option<f32>,
    captured_frames: usize,
    notes: Vec<String>,
    restored_profile: Option<String>,
}

#[derive(Debug, Clone)]
struct SelectedTestEndpoints {
    microphone: ExternalDeviceInfo,
    speaker: ExternalDeviceInfo,
    microphone_channels: u16,
    speaker_channels: u16,
}

pub(crate) async fn run_internal_latency_test_command(
    client: &IpcClient,
    sample_rate: u32,
    buffer_frames: u32,
) -> Result<InternalLatencyTestReport, CliError> {
    validate_test_audio_config(sample_rate, buffer_frames)?;
    let previous_profile = request_status(client).await?.current_profile;
    let profile_path = make_test_profile_path();
    let phase_result = execute_test_phase(
        client,
        &profile_path,
        build_internal_test_profile(sample_rate, buffer_frames),
        sample_rate,
        2,
        2,
    )
    .await;

    let restore_result = restore_profile(client, previous_profile.as_deref()).await;
    let cleanup_result = match fs::remove_file(&profile_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };

    if let Err(error) = cleanup_result {
        return Err(CliError::WithExit {
            message: format!(
                "test cleanup failed for '{}': {error}",
                profile_path.display()
            ),
            exit_code: ExitCode::ApplyFailed,
        });
    }

    match (phase_result, restore_result) {
        (Ok(internal), Ok(())) => {
            let internal_report = probe_report(internal);
            let mut notes = Vec::new();
            notes.push(
                "Measures the internal MARS data plane (`mars.vout` -> daemon render loop -> `mars.vin`) only; excludes app-side CoreAudio host streams and any speaker/microphone path."
                    .to_string(),
            );
            if internal_report.correlation_score < 0.2 {
                notes.push(
                    "Correlation was weak; retry before trusting the latency number.".to_string(),
                );
            }
            if internal_report.latency_frames < 0 {
                notes.push(
                    "Measured internal latency was not positive; treat the reported number as low-confidence and retry."
                        .to_string(),
                );
            } else if internal_report.latency_frames == 0 {
                notes.push(
                    "Measured sample offset was 0 frames. That means the current internal route did not add extra sample buffering beyond the daemon's scheduling cadence."
                        .to_string(),
                );
            }
            Ok(InternalLatencyTestReport {
                context: internal_latency_test_context(),
                sample_rate,
                buffer_frames,
                virtual_system_output_uid: coreaudio_uid(TEST_VIRTUAL_OUTPUT_UID),
                virtual_capture_input_uid: coreaudio_uid(TEST_VIRTUAL_INPUT_UID),
                internal: internal_report,
                notes,
                restored_profile: previous_profile,
            })
        }
        (Ok(_internal), Err(restore_error)) => Err(CliError::WithExit {
            message: format!(
                "test finished but restoring the previous profile failed: {restore_error}"
            ),
            exit_code: ExitCode::ApplyFailed,
        }),
        (Err(test_error), Ok(())) => Err(test_error),
        (Err(test_error), Err(restore_error)) => Err(CliError::WithExit {
            message: format!("{test_error}; restore failed: {restore_error}"),
            exit_code: ExitCode::ApplyFailed,
        }),
    }
}

pub(crate) async fn run_route_test_command(
    client: &IpcClient,
    mic_selector: Option<&str>,
    speaker_selector: Option<&str>,
    sample_rate: u32,
    buffer_frames: u32,
) -> Result<RouteTestReport, CliError> {
    let selected =
        select_test_endpoints(mic_selector, speaker_selector, sample_rate, buffer_frames)?;
    let previous_profile = request_status(client).await?.current_profile;
    let profile_path = make_test_profile_path();
    let route_profile = build_route_test_profile(
        sample_rate,
        buffer_frames,
        &selected.microphone,
        selected.microphone_channels,
        &selected.speaker,
        selected.speaker_channels,
    );
    let monitor_result = async {
        write_profile(&profile_path, &route_profile)?;
        apply_profile_request(client, &profile_path).await?;
        sleep(TEST_DEVICE_READY_INTERVAL).await;
        let monitor_request = VinRingMonitorRequest {
            vin_uid: TEST_VIRTUAL_INPUT_UID.to_string(),
            sample_rate,
            vin_channels: selected.microphone_channels,
            buffer_frames,
            timeout: ROUTE_LISTEN_DURATION,
        };
        let monitor_result =
            tokio::task::spawn_blocking(move || monitor_vin_ring_signal(&monitor_request))
                .await
                .map_err(|error| CliError::WithExit {
                    message: format!("route test monitor task failed: {error}"),
                    exit_code: ExitCode::ApplyFailed,
                })?
                .map_err(coreaudio_to_cli_error)?;
        let status = request_status(client).await?;
        Ok::<
            (
                mars_types::DaemonStatus,
                mars_coreaudio::VinRingMonitorResult,
            ),
            CliError,
        >((status, monitor_result))
    }
    .await;

    let restore_result = restore_profile(client, previous_profile.as_deref()).await;
    let cleanup_result = match fs::remove_file(&profile_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = cleanup_result {
        return Err(CliError::WithExit {
            message: format!(
                "test cleanup failed for '{}': {error}",
                profile_path.display()
            ),
            exit_code: ExitCode::ApplyFailed,
        });
    }

    match (monitor_result, restore_result) {
        (Ok((status, monitor)), Ok(())) => {
            let capture_signal_detected = monitor.peak >= TEST_SIGNAL_PEAK_THRESHOLD;
            if !capture_signal_detected {
                return Err(CliError::WithExit {
                    message: format!(
                        "route test did not observe enough microphone signal (peak={:.3}, rms={:.3})\nselected microphone: {} ({})\nselected speaker: {} ({})\nspeak louder, clap near the microphone, or override devices with `mars test --route --mic {} --speaker {}`.",
                        monitor.peak,
                        monitor.rms,
                        selected.microphone.name,
                        selected.microphone.uid,
                        selected.speaker.name,
                        selected.speaker.uid,
                        selected.microphone.uid,
                        selected.speaker.uid,
                    ),
                    exit_code: ExitCode::ApplyFailed,
                });
            }

            let mut notes = Vec::new();
            notes.push(
                "Signal detection is verified through the virtual capture path; physical speaker audibility is inferred from the connected external output stream, not directly microphone-sampled."
                    .to_string(),
            );
            if monitor.peak < 0.02 {
                notes.push(
                    "Detected signal level was low; if monitoring still sounds wrong, retry with a louder source or the microphone closer to the speaker."
                        .to_string(),
                );
            }
            Ok(RouteTestReport {
                context: route_test_context(
                    &selected.microphone,
                    selected.microphone_channels,
                    &selected.speaker,
                    selected.speaker_channels,
                ),
                sample_rate,
                buffer_frames,
                listen_ms: ROUTE_LISTEN_DURATION.as_millis() as u64,
                microphone: selected.microphone,
                speaker: selected.speaker,
                virtual_capture_input_uid: coreaudio_uid(TEST_VIRTUAL_INPUT_UID),
                external_input_connected: status.external_runtime.connected_inputs > 0,
                external_output_connected: status.external_runtime.connected_outputs > 0,
                capture_signal_detected,
                peak_dbfs: linear_to_dbfs(monitor.peak),
                rms_dbfs: linear_to_dbfs(monitor.rms),
                captured_frames: monitor.captured_frames,
                notes,
                restored_profile: previous_profile,
            })
        }
        (Ok((_status, _monitor)), Err(restore_error)) => Err(CliError::WithExit {
            message: format!(
                "route test finished but restoring the previous profile failed: {restore_error}"
            ),
            exit_code: ExitCode::ApplyFailed,
        }),
        (Err(test_error), Ok(())) => Err(test_error),
        (Err(test_error), Err(restore_error)) => Err(CliError::WithExit {
            message: format!("{test_error}; restore failed: {restore_error}"),
            exit_code: ExitCode::ApplyFailed,
        }),
    }
}

fn probe_report(result: LoopbackProbeResult) -> ProbeReport {
    ProbeReport {
        latency_frames: result.latency_frames,
        latency_ms: result.latency_ms,
        correlation_score: result.correlation_score,
        output_signal_frames: result.output_signal_frames,
        captured_frames: result.captured_frames,
    }
}

fn internal_latency_test_context() -> TestContextReport {
    TestContextReport {
        mode: "internal_latency".to_string(),
        description: "Measures internal sample-offset latency through the MARS data plane."
            .to_string(),
        signal: "Injects a deterministic impulse+chirp probe into the temporary virtual output and correlates the captured temporary virtual input."
            .to_string(),
        source: TestEndpointReport {
            kind: "virtual_output".to_string(),
            logical_id: Some(TEST_VIRTUAL_OUTPUT_ID.to_string()),
            name: TEST_VIRTUAL_OUTPUT_NAME.to_string(),
            uid: coreaudio_uid(TEST_VIRTUAL_OUTPUT_UID),
            channels: 2,
        },
        sinks: vec![TestEndpointReport {
            kind: "virtual_input".to_string(),
            logical_id: Some(TEST_VIRTUAL_INPUT_ID.to_string()),
            name: TEST_VIRTUAL_INPUT_NAME.to_string(),
            uid: coreaudio_uid(TEST_VIRTUAL_INPUT_UID),
            channels: 2,
        }],
        paths: vec![
            "virtual output ring -> daemon render loop -> virtual input ring".to_string(),
        ],
        includes: vec![
            "MARS shared-memory ring transport".to_string(),
            "daemon render scheduling and graph traversal".to_string(),
            "internal sample buffering between virtual output and virtual input".to_string(),
        ],
        excludes: vec![
            "app-side CoreAudio host streams".to_string(),
            "physical speaker/microphone latency".to_string(),
            "external device I/O".to_string(),
        ],
    }
}

fn route_test_context(
    microphone: &ExternalDeviceInfo,
    microphone_channels: u16,
    speaker: &ExternalDeviceInfo,
    speaker_channels: u16,
) -> TestContextReport {
    TestContextReport {
        mode: "route_signal_detection".to_string(),
        description:
            "Verifies that microphone signal enters MARS and reaches both the selected speaker route and the virtual capture path."
                .to_string(),
        signal: "Listens for live microphone input such as speech or a clap.".to_string(),
        source: TestEndpointReport {
            kind: "external_input".to_string(),
            logical_id: Some(TEST_MIC_ID.to_string()),
            name: microphone.name.clone(),
            uid: microphone.uid.clone(),
            channels: microphone_channels,
        },
        sinks: vec![
            TestEndpointReport {
                kind: "external_output".to_string(),
                logical_id: Some(TEST_SPEAKER_ID.to_string()),
                name: speaker.name.clone(),
                uid: speaker.uid.clone(),
                channels: speaker_channels,
            },
            TestEndpointReport {
                kind: "virtual_input".to_string(),
                logical_id: Some(TEST_VIRTUAL_INPUT_ID.to_string()),
                name: TEST_VIRTUAL_INPUT_NAME.to_string(),
                uid: coreaudio_uid(TEST_VIRTUAL_INPUT_UID),
                channels: microphone_channels,
            },
        ],
        paths: vec![
            "microphone -> daemon render loop -> speaker".to_string(),
            "microphone -> daemon render loop -> virtual capture".to_string(),
        ],
        includes: vec![
            "selected microphone external input stream".to_string(),
            "selected speaker external output stream".to_string(),
            "virtual capture sink visibility".to_string(),
        ],
        excludes: vec![
            "a numeric latency measurement".to_string(),
            "speaker audibility measured back through a microphone".to_string(),
        ],
    }
}

fn validate_test_audio_config(sample_rate: u32, buffer_frames: u32) -> Result<(), CliError> {
    if sample_rate == 0 {
        return Err(CliError::WithExit {
            message: "--sample-rate must be > 0".to_string(),
            exit_code: ExitCode::InvalidInput,
        });
    }
    if buffer_frames == 0 {
        return Err(CliError::WithExit {
            message: "--buffer-frames must be > 0".to_string(),
            exit_code: ExitCode::InvalidInput,
        });
    }
    Ok(())
}

fn select_test_endpoints(
    mic_selector: Option<&str>,
    speaker_selector: Option<&str>,
    sample_rate: u32,
    buffer_frames: u32,
) -> Result<SelectedTestEndpoints, CliError> {
    validate_test_audio_config(sample_rate, buffer_frames)?;

    let inventory = list_device_inventory().map_err(coreaudio_to_cli_error)?;
    let default_mic =
        default_device_info(StreamDirection::Input).map_err(coreaudio_to_cli_error)?;
    let default_speaker =
        default_device_info(StreamDirection::Output).map_err(coreaudio_to_cli_error)?;
    let preferred_mic = if mic_selector.is_none() && speaker_selector.is_none() {
        preferred_builtin_device(&inventory.inputs, BUILTIN_MIC_UID)
            .unwrap_or_else(|| default_mic.clone())
    } else {
        default_mic.clone()
    };
    let preferred_speaker = if mic_selector.is_none() && speaker_selector.is_none() {
        preferred_builtin_device(&inventory.outputs, BUILTIN_SPEAKER_UID)
            .unwrap_or_else(|| default_speaker.clone())
    } else {
        default_speaker.clone()
    };
    let microphone = resolve_test_device(
        &inventory.inputs,
        mic_selector,
        &preferred_mic,
        "microphone",
    )?;
    let speaker = resolve_test_device(
        &inventory.outputs,
        speaker_selector,
        &preferred_speaker,
        "speaker",
    )?;
    let microphone_channels = select_test_channels(
        &microphone.uid,
        StreamDirection::Input,
        sample_rate,
        &[1, 2],
    )?;
    let speaker_channels =
        select_test_channels(&speaker.uid, StreamDirection::Output, sample_rate, &[2, 1])?;

    Ok(SelectedTestEndpoints {
        microphone,
        speaker,
        microphone_channels,
        speaker_channels,
    })
}

fn resolve_test_device(
    devices: &[ExternalDeviceInfo],
    selector: Option<&str>,
    default_device: &ExternalDeviceInfo,
    label: &str,
) -> Result<ExternalDeviceInfo, CliError> {
    let Some(selector) = selector else {
        return Ok(default_device.clone());
    };

    let matches = devices
        .iter()
        .filter(|device| device.uid == selector || device.name == selector)
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [device] => Ok(device.clone()),
        [] => Err(CliError::WithExit {
            message: format!(
                "{label} '{selector}' not found (use `mars devices --json` to inspect available UIDs)"
            ),
            exit_code: ExitCode::InvalidInput,
        }),
        _ => Err(CliError::WithExit {
            message: format!("{label} selector '{selector}' is ambiguous; match by UID instead"),
            exit_code: ExitCode::InvalidInput,
        }),
    }
}

fn preferred_builtin_device(
    devices: &[ExternalDeviceInfo],
    builtin_uid: &str,
) -> Option<ExternalDeviceInfo> {
    devices
        .iter()
        .find(|device| device.uid == builtin_uid)
        .cloned()
}

fn linear_to_dbfs(value: f32) -> Option<f32> {
    if value <= 0.0 {
        None
    } else {
        Some(20.0 * value.log10())
    }
}

fn format_dbfs(value: Option<f32>) -> String {
    value
        .map(|dbfs| format!("{dbfs:.1} dBFS"))
        .unwrap_or_else(|| "-inf dBFS".to_string())
}

fn format_test_endpoint(endpoint: &TestEndpointReport) -> String {
    match endpoint.logical_id.as_deref() {
        Some(logical_id) => format!(
            "{} [{} id={} uid={} channels={}]",
            endpoint.name, endpoint.kind, logical_id, endpoint.uid, endpoint.channels
        ),
        None => format!(
            "{} [{} uid={} channels={}]",
            endpoint.name, endpoint.kind, endpoint.uid, endpoint.channels
        ),
    }
}

pub(crate) fn format_internal_latency_test_report(report: &InternalLatencyTestReport) -> String {
    let mut lines = vec![
        format!("Test: {}", report.context.mode),
        format!("What: {}", report.context.description),
        format!("Signal: {}", report.context.signal),
        format!("Input: {}", format_test_endpoint(&report.context.source)),
        format!(
            "Output: {}",
            report
                .context
                .sinks
                .iter()
                .map(format_test_endpoint)
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        format!("Path: {}", report.context.paths.join(" ; ")),
        format!("Includes: {}", report.context.includes.join("; ")),
        format!("Excludes: {}", report.context.excludes.join("; ")),
        format!(
            "Config: sample_rate={}Hz buffer_frames={}",
            report.sample_rate, report.buffer_frames
        ),
        format!(
            "Result: latency={:.2}ms frames={} correlation={:.3}",
            report.internal.latency_ms,
            report.internal.latency_frames,
            report.internal.correlation_score
        ),
        format!(
            "Samples: probe_frames={} captured_frames={}",
            report.internal.output_signal_frames, report.internal.captured_frames
        ),
    ];
    if !report.notes.is_empty() {
        lines.push(format!("Notes: {}", report.notes.join(" ")));
    }
    if let Some(restored_profile) = report.restored_profile.as_deref() {
        lines.push(format!("Restored profile: {restored_profile}"));
    }
    lines.join("\n")
}

pub(crate) fn format_route_test_report(report: &RouteTestReport) -> String {
    let mut lines = vec![
        format!("Test: {}", report.context.mode),
        format!("What: {}", report.context.description),
        format!("Signal: {}", report.context.signal),
        format!("Input: {}", format_test_endpoint(&report.context.source)),
        format!(
            "Outputs: {}",
            report
                .context
                .sinks
                .iter()
                .map(format_test_endpoint)
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        format!("Paths: {}", report.context.paths.join(" ; ")),
        format!("Includes: {}", report.context.includes.join("; ")),
        format!("Excludes: {}", report.context.excludes.join("; ")),
        format!(
            "Config: sample_rate={}Hz buffer_frames={} listen={}ms",
            report.sample_rate, report.buffer_frames, report.listen_ms
        ),
        format!(
            "Result: signal_detected={} peak={} rms={} captured_frames={}",
            report.capture_signal_detected,
            format_dbfs(report.peak_dbfs),
            format_dbfs(report.rms_dbfs),
            report.captured_frames
        ),
        format!(
            "Connectivity: external_input_connected={} external_output_connected={}",
            report.external_input_connected, report.external_output_connected
        ),
    ];
    if !report.notes.is_empty() {
        lines.push(format!("Notes: {}", report.notes.join(" ")));
    }
    if let Some(restored_profile) = report.restored_profile.as_deref() {
        lines.push(format!("Restored profile: {restored_profile}"));
    }
    lines.join("\n")
}

fn build_internal_test_profile(sample_rate: u32, buffer_frames: u32) -> Profile {
    let mut profile = Profile {
        version: 1,
        name: Some("mars-test-internal-latency".to_string()),
        description: Some("Temporary internal loopback profile for `mars test`.".to_string()),
        audio: AudioConfig {
            sample_rate: AutoOrU32::Value(sample_rate),
            channels: AutoOrU16::Value(2),
            buffer_frames,
            format: Default::default(),
            latency_mode: Default::default(),
        },
        virtual_devices: VirtualDevices {
            outputs: vec![VirtualOutputDevice {
                id: TEST_VIRTUAL_OUTPUT_ID.to_string(),
                name: TEST_VIRTUAL_OUTPUT_NAME.to_string(),
                channels: Some(2),
                uid: Some(TEST_VIRTUAL_OUTPUT_UID.to_string()),
                hidden: false,
            }],
            inputs: vec![VirtualInputDevice {
                id: TEST_VIRTUAL_INPUT_ID.to_string(),
                name: TEST_VIRTUAL_INPUT_NAME.to_string(),
                channels: Some(2),
                uid: Some(TEST_VIRTUAL_INPUT_UID.to_string()),
                mix: None,
            }],
        },
        buses: Vec::new(),
        external: ExternalDevices::default(),
        pipes: Vec::new(),
        policy: Policy::default(),
    };

    profile.pipes.push(Pipe {
        from: TEST_VIRTUAL_OUTPUT_ID.to_string(),
        to: TEST_VIRTUAL_INPUT_ID.to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });

    profile
}

fn build_route_test_profile(
    sample_rate: u32,
    buffer_frames: u32,
    microphone: &ExternalDeviceInfo,
    microphone_channels: u16,
    speaker: &ExternalDeviceInfo,
    speaker_channels: u16,
) -> Profile {
    let mut profile = Profile {
        version: 1,
        name: Some("mars-test-route".to_string()),
        description: Some(
            "Temporary microphone-to-speaker route test profile for `mars test --route`."
                .to_string(),
        ),
        audio: AudioConfig {
            sample_rate: AutoOrU32::Value(sample_rate),
            channels: AutoOrU16::Value(2),
            buffer_frames,
            format: Default::default(),
            latency_mode: Default::default(),
        },
        virtual_devices: VirtualDevices {
            outputs: vec![VirtualOutputDevice {
                id: TEST_VIRTUAL_OUTPUT_ID.to_string(),
                name: TEST_VIRTUAL_OUTPUT_NAME.to_string(),
                channels: Some(speaker_channels),
                uid: Some(TEST_VIRTUAL_OUTPUT_UID.to_string()),
                hidden: false,
            }],
            inputs: vec![VirtualInputDevice {
                id: TEST_VIRTUAL_INPUT_ID.to_string(),
                name: TEST_VIRTUAL_INPUT_NAME.to_string(),
                channels: Some(microphone_channels),
                uid: Some(TEST_VIRTUAL_INPUT_UID.to_string()),
                mix: None,
            }],
        },
        buses: Vec::new(),
        external: ExternalDevices::default(),
        pipes: Vec::new(),
        policy: Policy::default(),
    };

    profile.external.inputs.push(ExternalInput {
        id: TEST_MIC_ID.to_string(),
        r#match: mars_types::DeviceMatch {
            uid: Some(microphone.uid.clone()),
            name: None,
            name_regex: None,
            manufacturer: None,
            transport: None,
        },
        channels: Some(microphone_channels),
    });
    profile.external.outputs.push(ExternalOutput {
        id: TEST_SPEAKER_ID.to_string(),
        r#match: mars_types::DeviceMatch {
            uid: Some(speaker.uid.clone()),
            name: None,
            name_regex: None,
            manufacturer: None,
            transport: None,
        },
        channels: Some(speaker_channels),
    });
    profile.pipes.push(Pipe {
        from: TEST_VIRTUAL_OUTPUT_ID.to_string(),
        to: TEST_SPEAKER_ID.to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    profile.pipes.push(Pipe {
        from: TEST_MIC_ID.to_string(),
        to: TEST_VIRTUAL_INPUT_ID.to_string(),
        enabled: true,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });

    profile
}

fn make_test_profile_path() -> PathBuf {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "mars-test-{}-{timestamp_ms}.yaml",
        std::process::id()
    ))
}

async fn execute_test_phase(
    client: &IpcClient,
    profile_path: &Path,
    profile: Profile,
    sample_rate: u32,
    output_channels: u16,
    input_channels: u16,
) -> Result<LoopbackProbeResult, CliError> {
    write_profile(profile_path, &profile)?;
    apply_profile_request(client, profile_path).await?;
    sleep(TEST_DEVICE_READY_INTERVAL).await;
    let request = VinRingLoopbackProbeRequest {
        output_uid: TEST_VIRTUAL_OUTPUT_UID.to_string(),
        vin_uid: TEST_VIRTUAL_INPUT_UID.to_string(),
        sample_rate,
        output_channels,
        vin_channels: input_channels,
        buffer_frames: profile.audio.buffer_frames,
        timeout: TEST_PROBE_TIMEOUT,
    };
    tokio::task::spawn_blocking(move || measure_vin_ring_loopback_latency(&request))
        .await
        .map_err(|error| CliError::WithExit {
            message: format!("loopback probe task failed: {error}"),
            exit_code: ExitCode::ApplyFailed,
        })?
        .map_err(coreaudio_to_cli_error)
}

fn write_profile(path: &Path, profile: &Profile) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_yaml::to_string(profile)?)?;
    Ok(())
}

async fn apply_profile_request(client: &IpcClient, profile_path: &Path) -> Result<(), CliError> {
    let response = client
        .send(DaemonRequest::Apply(ApplyRequest {
            profile_path: profile_path.display().to_string(),
            no_delete: false,
            dry_run: false,
            timeout_ms: TEST_APPLY_TIMEOUT_MS,
        }))
        .await
        .map_err(ipc_to_cli_error)?;
    let DaemonResponse::Apply(result) = response else {
        return Err(CliError::WithExit {
            message: "unexpected daemon response for apply".to_string(),
            exit_code: ExitCode::DaemonCommunication,
        });
    };
    if result.errors.is_empty() {
        Ok(())
    } else {
        Err(CliError::WithExit {
            message: format!("test profile apply failed: {}", result.errors.join("; ")),
            exit_code: ExitCode::ApplyFailed,
        })
    }
}

async fn request_status(client: &IpcClient) -> Result<mars_types::DaemonStatus, CliError> {
    let response = client
        .send(DaemonRequest::Status)
        .await
        .map_err(ipc_to_cli_error)?;
    let DaemonResponse::Status(result) = response else {
        return Err(CliError::WithExit {
            message: "unexpected daemon response for status".to_string(),
            exit_code: ExitCode::DaemonCommunication,
        });
    };
    Ok(result)
}

async fn restore_profile(client: &IpcClient, profile_path: Option<&str>) -> Result<(), CliError> {
    match profile_path {
        Some(profile_path) => {
            let path = Path::new(profile_path);
            if !path.exists() {
                return Err(CliError::WithExit {
                    message: format!("previous profile no longer exists: '{}'", path.display()),
                    exit_code: ExitCode::ApplyFailed,
                });
            }
            apply_profile_request(client, path).await
        }
        None => {
            let response = client
                .send(DaemonRequest::Clear(ClearRequest {
                    keep_devices: false,
                }))
                .await
                .map_err(ipc_to_cli_error)?;
            let DaemonResponse::Clear(result) = response else {
                return Err(CliError::WithExit {
                    message: "unexpected daemon response for clear".to_string(),
                    exit_code: ExitCode::DaemonCommunication,
                });
            };
            if result.errors.is_empty() {
                Ok(())
            } else {
                Err(CliError::WithExit {
                    message: format!("failed to clear test profile: {}", result.errors.join("; ")),
                    exit_code: ExitCode::ApplyFailed,
                })
            }
        }
    }
}

fn select_test_channels(
    uid: &str,
    direction: StreamDirection,
    sample_rate: u32,
    preferred: &[u16],
) -> Result<u16, CliError> {
    match resolve_channel_count(uid, direction, sample_rate, preferred) {
        Ok(channels) => Ok(channels),
        Err(mars_coreaudio::CoreAudioError::UnsupportedChannelCount { .. }) => {
            let supported = supported_channel_counts(uid, direction, sample_rate)
                .map_err(coreaudio_to_cli_error)?;
            supported.last().copied().ok_or_else(|| CliError::WithExit {
                message: format!(
                    "no supported {} channel count found for uid '{}' at {} Hz",
                    direction.as_str(),
                    uid,
                    sample_rate
                ),
                exit_code: ExitCode::InvalidInput,
            })
        }
        Err(error) => Err(coreaudio_to_cli_error(error)),
    }
}

fn coreaudio_uid(uid: &str) -> String {
    format!("{COREAUDIO_UID_PREFIX}{uid}")
}

fn coreaudio_to_cli_error(error: mars_coreaudio::CoreAudioError) -> CliError {
    let exit_code = match error {
        mars_coreaudio::CoreAudioError::DefaultDeviceUnavailable { .. }
        | mars_coreaudio::CoreAudioError::DeviceNotFound { .. }
        | mars_coreaudio::CoreAudioError::UnsupportedChannelCount { .. }
        | mars_coreaudio::CoreAudioError::UnsupportedStreamConfig { .. } => ExitCode::InvalidInput,
        _ => ExitCode::ApplyFailed,
    };
    CliError::WithExit {
        message: error.to_string(),
        exit_code,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use mars_types::ExternalDeviceInfo;

    use super::{
        InternalLatencyTestReport, ProbeReport, TEST_MIC_ID, TEST_SPEAKER_ID,
        TEST_VIRTUAL_INPUT_ID, TEST_VIRTUAL_INPUT_UID, TEST_VIRTUAL_OUTPUT_ID,
        TEST_VIRTUAL_OUTPUT_UID, build_internal_test_profile, build_route_test_profile,
        coreaudio_uid, format_internal_latency_test_report, internal_latency_test_context,
        probe_report,
    };

    fn device(uid: &str, name: &str, channels: u16) -> ExternalDeviceInfo {
        ExternalDeviceInfo {
            uid: uid.to_string(),
            name: name.to_string(),
            manufacturer: None,
            transport: None,
            channels,
            sample_rates: vec![48_000],
        }
    }

    #[test]
    fn internal_test_profile_is_direct_loopback_only() {
        let profile = build_internal_test_profile(48_000, 256);

        assert!(profile.external.inputs.is_empty());
        assert!(profile.external.outputs.is_empty());
        assert_eq!(
            profile.virtual_devices.outputs[0].id,
            TEST_VIRTUAL_OUTPUT_ID
        );
        assert_eq!(
            profile.virtual_devices.outputs[0].uid.as_deref(),
            Some(TEST_VIRTUAL_OUTPUT_UID)
        );
        assert_eq!(profile.virtual_devices.inputs[0].id, TEST_VIRTUAL_INPUT_ID);
        assert_eq!(
            profile.virtual_devices.inputs[0].uid.as_deref(),
            Some(TEST_VIRTUAL_INPUT_UID)
        );
        assert_eq!(profile.pipes.len(), 1);
        assert_eq!(profile.pipes[0].from, TEST_VIRTUAL_OUTPUT_ID);
        assert_eq!(profile.pipes[0].to, TEST_VIRTUAL_INPUT_ID);
    }

    #[test]
    fn route_test_profile_uses_requested_mic_and_speaker() {
        let microphone = device("mic-uid", "Built-in Mic", 1);
        let speaker = device("speaker-uid", "Built-in Speakers", 2);
        let profile = build_route_test_profile(48_000, 256, &microphone, 1, &speaker, 2);

        assert_eq!(profile.external.inputs.len(), 1);
        assert_eq!(profile.external.inputs[0].id, TEST_MIC_ID);
        assert_eq!(
            profile.external.inputs[0].r#match.uid.as_deref(),
            Some("mic-uid")
        );
        assert_eq!(profile.external.outputs.len(), 1);
        assert_eq!(profile.external.outputs[0].id, TEST_SPEAKER_ID);
        assert_eq!(
            profile.external.outputs[0].r#match.uid.as_deref(),
            Some("speaker-uid")
        );
        assert_eq!(profile.pipes.len(), 2);
        assert_eq!(profile.pipes[0].from, TEST_VIRTUAL_OUTPUT_ID);
        assert_eq!(profile.pipes[0].to, TEST_SPEAKER_ID);
        assert_eq!(profile.pipes[1].from, TEST_MIC_ID);
        assert_eq!(profile.pipes[1].to, TEST_VIRTUAL_INPUT_ID);
    }

    #[test]
    fn probe_report_copies_probe_metrics() {
        let report = probe_report(mars_coreaudio::LoopbackProbeResult {
            latency_frames: 128,
            latency_ms: 2.666,
            correlation_score: 0.91,
            output_signal_frames: 7_200,
            captured_frames: 8_000,
        });

        assert_eq!(report.latency_frames, 128);
        assert_eq!(report.output_signal_frames, 7_200);
        assert_eq!(report.captured_frames, 8_000);
        assert!(report.correlation_score > 0.9);
    }

    #[test]
    fn internal_latency_report_human_output_describes_test_context() {
        let rendered = format_internal_latency_test_report(&InternalLatencyTestReport {
            context: internal_latency_test_context(),
            sample_rate: 48_000,
            buffer_frames: 256,
            virtual_system_output_uid: coreaudio_uid(TEST_VIRTUAL_OUTPUT_UID),
            virtual_capture_input_uid: coreaudio_uid(TEST_VIRTUAL_INPUT_UID),
            internal: ProbeReport {
                latency_frames: 2048,
                latency_ms: 42.666,
                correlation_score: 1.0,
                output_signal_frames: 55_200,
                captured_frames: 46_848,
            },
            notes: vec!["example note".to_string()],
            restored_profile: Some("demo".to_string()),
        });

        assert!(rendered.contains("Test: internal_latency"));
        assert!(rendered.contains("Input: Bus: Test System"));
        assert!(rendered.contains("Output: Mix: Test Capture"));
        assert!(
            rendered
                .contains("Path: virtual output ring -> daemon render loop -> virtual input ring")
        );
        assert!(rendered.contains("Result: latency=42.67ms frames=2048 correlation=1.000"));
        assert!(rendered.contains("Notes: example note"));
        assert!(rendered.contains("Restored profile: demo"));
    }
}

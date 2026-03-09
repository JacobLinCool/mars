#![forbid(unsafe_code)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use mars_coreaudio::{
    LoopbackProbeResult, StreamDirection, VinRingLoopbackProbeRequest, VinRingMonitorRequest,
    default_device_info, list_device_inventory, measure_vin_ring_loopback_latency,
    monitor_vin_ring_signal, resolve_channel_count, supported_channel_counts,
};
use mars_ipc::{DaemonRequest, DaemonResponse, IpcClient, LogRequest, LogResponse};
use mars_profile::{TemplateKind, render_template};
use mars_types::{
    ApplyRequest, AudioConfig, AutoOrU16, AutoOrU32, ClearRequest, DEFAULT_PROFILE_DIR_RELATIVE,
    DEFAULT_SOCKET_PATH_RELATIVE, ExitCode, ExternalDeviceInfo, ExternalDevices, ExternalInput,
    ExternalOutput, Pipe, PlanRequest, Policy, Profile, ValidateRequest, VirtualDevices,
    VirtualInputDevice, VirtualOutputDevice,
};
use serde::Serialize;
use thiserror::Error;
use tokio::time::sleep;

const PROFILE_NAME_MAX_LEN: usize = 64;
const DAEMON_PING_RETRY_COUNT: usize = 50;
const DAEMON_PING_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const LOG_FOLLOW_POLL_INTERVAL: Duration = Duration::from_secs(1);
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
const TEST_ROUTE_LISTEN_DURATION: Duration = Duration::from_secs(5);
const TEST_DEVICE_READY_INTERVAL: Duration = Duration::from_millis(100);
const COREAUDIO_UID_PREFIX: &str = "coreaudio:";
const TEST_SIGNAL_PEAK_THRESHOLD: f32 = 0.008;

#[derive(Debug, Parser)]
#[command(name = "mars")]
#[command(about = "MARS (macOS Audio Router Service) CLI")]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Create {
        profile_name: String,
        #[arg(long, default_value = "default")]
        template: String,
        #[arg(long)]
        force: bool,
    },
    Open {
        profile_name: String,
        #[arg(long)]
        editor: Option<String>,
        #[arg(long)]
        print_path: bool,
    },
    Apply {
        profile_name: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        no_delete: bool,
        #[arg(long, default_value_t = 5000)]
        timeout: u64,
    },
    Clear {
        #[arg(long)]
        keep_devices: bool,
    },
    Validate {
        profile_name: String,
    },
    Plan {
        profile_name: String,
        #[arg(long)]
        no_delete: bool,
    },
    Status,
    Devices,
    Test {
        #[arg(
            long,
            help = "Run the mic-to-speaker route check instead of the default internal latency probe"
        )]
        route: bool,
        #[arg(
            long,
            help = "Microphone device UID or exact name; only used with --route"
        )]
        mic: Option<String>,
        #[arg(
            long,
            help = "Speaker device UID or exact name; only used with --route"
        )]
        speaker: Option<String>,
        #[arg(long, default_value_t = 48_000)]
        sample_rate: u32,
        #[arg(long, default_value_t = 256)]
        buffer_frames: u32,
    },
    Logs {
        #[arg(long)]
        follow: bool,
    },
    Doctor,
}

#[derive(Debug, Error)]
enum CliError {
    #[error("{message}")]
    WithExit {
        message: String,
        exit_code: ExitCode,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

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
struct InternalLatencyTestReport {
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
struct RouteTestReport {
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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let exit_code = tokio::select! {
        result = run(cli) => {
            match result {
                Ok(code) => code,
                Err(error) => {
                    let (message, code) = match error {
                        CliError::WithExit { message, exit_code } => (message, exit_code),
                        other => (other.to_string(), ExitCode::DaemonCommunication),
                    };
                    eprintln!("{message}");
                    code
                }
            }
        }
        _ = tokio::signal::ctrl_c() => ExitCode::Interrupted,
    };
    std::process::exit(exit_code.as_i32());
}

async fn run(cli: Cli) -> Result<ExitCode, CliError> {
    match cli.command {
        Commands::Create {
            profile_name,
            template,
            force,
        } => {
            let template_kind =
                TemplateKind::parse(&template).ok_or_else(|| CliError::WithExit {
                    message: format!("unsupported template '{template}'"),
                    exit_code: ExitCode::InvalidInput,
                })?;

            let path = profile_path(&profile_name)?;
            if path.exists() && !force {
                return Err(CliError::WithExit {
                    message: format!(
                        "profile already exists: '{}' (use --force to overwrite)",
                        path.display()
                    ),
                    exit_code: ExitCode::InvalidInput,
                });
            }

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, render_template(&profile_name, template_kind))?;

            let payload = serde_json::json!({
                "profile": profile_name,
                "path": path.display().to_string(),
                "created": true,
            });
            print_output(cli.json, &payload, || {
                format!("created '{}'", path.display())
            })?;
            Ok(ExitCode::Success)
        }
        Commands::Open {
            profile_name,
            editor,
            print_path,
        } => {
            let path = profile_path(&profile_name)?;
            if !path.exists() {
                return Err(CliError::WithExit {
                    message: format!("profile not found: '{}'", path.display()),
                    exit_code: ExitCode::InvalidInput,
                });
            }

            if print_path {
                let payload = serde_json::json!({ "path": path.display().to_string() });
                print_output(cli.json, &payload, || path.display().to_string())?;
                return Ok(ExitCode::Success);
            }

            let mut command = Command::new("open");
            if let Some(editor) = editor.as_ref() {
                command.arg("-a").arg(editor);
            }
            command.arg(&path);
            let status = command.status()?;
            if !status.success() {
                return Err(CliError::WithExit {
                    message: format!("failed to open profile: '{}'", path.display()),
                    exit_code: ExitCode::InvalidInput,
                });
            }

            let payload = serde_json::json!({
                "opened": true,
                "path": path.display().to_string(),
            });
            print_output(cli.json, &payload, || {
                format!("opened '{}'", path.display())
            })?;
            Ok(ExitCode::Success)
        }
        Commands::Apply {
            profile_name,
            dry_run,
            no_delete,
            timeout,
        } => {
            let profile_path = profile_path(&profile_name)?;
            if !profile_path.exists() {
                return Err(CliError::WithExit {
                    message: format!("profile not found: '{}'", profile_path.display()),
                    exit_code: ExitCode::InvalidInput,
                });
            }

            let client = daemon_client(Duration::from_millis(timeout)).await?;
            let request = ApplyRequest {
                profile_path: profile_path.display().to_string(),
                no_delete,
                dry_run,
                timeout_ms: timeout,
            };

            let response = client
                .send(DaemonRequest::Apply(request))
                .await
                .map_err(ipc_to_cli_error)?;
            let DaemonResponse::Apply(result) = response else {
                return Err(CliError::WithExit {
                    message: "unexpected daemon response for apply".to_string(),
                    exit_code: ExitCode::DaemonCommunication,
                });
            };

            print_output(cli.json, &result, || {
                format!(
                    "apply {} (changes: {}, warnings: {}, errors: {})",
                    if result.applied {
                        "succeeded"
                    } else {
                        "dry-run"
                    },
                    result.plan.changes.len(),
                    result.warnings.len(),
                    result.errors.len()
                )
            })?;

            let exit = if result.errors.is_empty() {
                ExitCode::Success
            } else {
                ExitCode::ApplyFailed
            };
            Ok(exit)
        }
        Commands::Clear { keep_devices } => {
            let client = daemon_client(Duration::from_millis(5000)).await?;
            let response = client
                .send(DaemonRequest::Clear(ClearRequest { keep_devices }))
                .await
                .map_err(ipc_to_cli_error)?;
            let DaemonResponse::Clear(result) = response else {
                return Err(CliError::WithExit {
                    message: "unexpected daemon response for clear".to_string(),
                    exit_code: ExitCode::DaemonCommunication,
                });
            };

            print_output(cli.json, &result, || "clear completed".to_string())?;
            Ok(ExitCode::Success)
        }
        Commands::Validate { profile_name } => {
            let profile_path = profile_path(&profile_name)?;
            if !profile_path.exists() {
                return Err(CliError::WithExit {
                    message: format!("profile not found: '{}'", profile_path.display()),
                    exit_code: ExitCode::InvalidInput,
                });
            }

            let client = daemon_client(Duration::from_millis(5000)).await?;
            let response = client
                .send(DaemonRequest::Validate(ValidateRequest {
                    profile_path: profile_path.display().to_string(),
                }))
                .await
                .map_err(ipc_to_cli_error)?;
            let DaemonResponse::Validate(result) = response else {
                return Err(CliError::WithExit {
                    message: "unexpected daemon response for validate".to_string(),
                    exit_code: ExitCode::DaemonCommunication,
                });
            };

            print_output(cli.json, &result, || {
                if result.valid {
                    "profile is valid".to_string()
                } else {
                    format!("profile is invalid: {}", result.errors.join("; "))
                }
            })?;

            Ok(if result.valid {
                ExitCode::Success
            } else {
                ExitCode::InvalidInput
            })
        }
        Commands::Plan {
            profile_name,
            no_delete,
        } => {
            let profile_path = profile_path(&profile_name)?;
            if !profile_path.exists() {
                return Err(CliError::WithExit {
                    message: format!("profile not found: '{}'", profile_path.display()),
                    exit_code: ExitCode::InvalidInput,
                });
            }

            let client = daemon_client(Duration::from_millis(5000)).await?;
            let response = client
                .send(DaemonRequest::Plan(PlanRequest {
                    profile_path: profile_path.display().to_string(),
                    no_delete,
                }))
                .await
                .map_err(ipc_to_cli_error)?;
            let DaemonResponse::Plan(result) = response else {
                return Err(CliError::WithExit {
                    message: "unexpected daemon response for plan".to_string(),
                    exit_code: ExitCode::DaemonCommunication,
                });
            };

            print_output(cli.json, &result, || {
                format!("plan generated: {} changes", result.changes.len())
            })?;
            Ok(ExitCode::Success)
        }
        Commands::Status => {
            let client = daemon_client(Duration::from_millis(5000)).await?;
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

            let current_profile = result.current_profile.clone();
            print_output(cli.json, &result, || {
                format!(
                    "running={} profile={} pipes={} driver_gen={} driver_pending={}",
                    result.running,
                    current_profile.unwrap_or_else(|| "<none>".to_string()),
                    result.graph_pipe_count,
                    result.driver.generation,
                    result.driver.pending_change
                )
            })?;
            Ok(ExitCode::Success)
        }
        Commands::Devices => {
            let client = daemon_client(Duration::from_millis(5000)).await?;
            let response = client
                .send(DaemonRequest::Devices)
                .await
                .map_err(ipc_to_cli_error)?;
            let DaemonResponse::Devices(result) = response else {
                return Err(CliError::WithExit {
                    message: "unexpected daemon response for devices".to_string(),
                    exit_code: ExitCode::DaemonCommunication,
                });
            };

            print_output(cli.json, &result, || {
                format!(
                    "inputs={} outputs={}",
                    result.inputs.len(),
                    result.outputs.len()
                )
            })?;
            Ok(ExitCode::Success)
        }
        Commands::Test {
            route,
            mic,
            speaker,
            sample_rate,
            buffer_frames,
        } => {
            let client = daemon_client(Duration::from_millis(TEST_APPLY_TIMEOUT_MS)).await?;
            if route {
                if !cli.json {
                    println!(
                        "Speak into the microphone or clap near it for the next {} seconds...",
                        TEST_ROUTE_LISTEN_DURATION.as_secs()
                    );
                }
                let report = run_route_test_command(
                    &client,
                    mic.as_deref(),
                    speaker.as_deref(),
                    sample_rate,
                    buffer_frames,
                )
                .await?;
                print_output(cli.json, &report, || format_route_test_report(&report))?;
            } else {
                if mic.is_some() || speaker.is_some() {
                    return Err(CliError::WithExit {
                        message: "`--mic` and `--speaker` require `mars test --route`".to_string(),
                        exit_code: ExitCode::InvalidInput,
                    });
                }
                let report =
                    run_internal_latency_test_command(&client, sample_rate, buffer_frames).await?;
                print_output(cli.json, &report, || {
                    format_internal_latency_test_report(&report)
                })?;
            }
            Ok(ExitCode::Success)
        }
        Commands::Logs { follow } => {
            if cli.json && follow {
                return Err(CliError::WithExit {
                    message: "logs --follow does not support --json output".to_string(),
                    exit_code: ExitCode::InvalidInput,
                });
            }

            let client = daemon_client(Duration::from_millis(5000)).await?;
            if follow {
                return follow_logs(&client).await;
            }

            let result = request_logs(&client, false, None, Some(200)).await?;
            print_output(cli.json, &result, || result.lines.join("\n"))?;
            Ok(ExitCode::Success)
        }
        Commands::Doctor => {
            let client = daemon_client(Duration::from_millis(5000)).await?;
            let response = client
                .send(DaemonRequest::Doctor)
                .await
                .map_err(ipc_to_cli_error)?;
            let DaemonResponse::Doctor(result) = response else {
                return Err(CliError::WithExit {
                    message: "unexpected daemon response for doctor".to_string(),
                    exit_code: ExitCode::DaemonCommunication,
                });
            };

            print_output(cli.json, &result, || {
                format!(
                    "driver_installed={} daemon_reachable={} mic_permission={} mic_source={} driver_version={} daemon_version={} driver_gen={} driver_pending={}",
                    result.driver_installed,
                    result.daemon_reachable,
                    result.microphone_permission_ok,
                    result.mic_permission_source,
                    result.driver_version.as_deref().unwrap_or("<unknown>"),
                    result.daemon_version,
                    result.driver.generation,
                    result.driver.pending_change
                )
            })?;

            Ok(if result.driver_installed && result.driver_compatible {
                ExitCode::Success
            } else {
                ExitCode::DriverUnavailable
            })
        }
    }
}

async fn run_internal_latency_test_command(
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

async fn run_route_test_command(
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
            timeout: TEST_ROUTE_LISTEN_DURATION,
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
                listen_ms: TEST_ROUTE_LISTEN_DURATION.as_millis() as u64,
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

fn format_internal_latency_test_report(report: &InternalLatencyTestReport) -> String {
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

fn format_route_test_report(report: &RouteTestReport) -> String {
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
        fallback: None,
        on_missing: None,
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
        fallback: None,
        on_missing: None,
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

fn print_output<T>(json: bool, value: &T, human: impl FnOnce() -> String) -> Result<(), CliError>
where
    T: Serialize,
{
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", human());
    }
    Ok(())
}

fn profile_path(profile_name: &str) -> Result<PathBuf, CliError> {
    validate_profile_name(profile_name)?;
    let profile_dir = default_profile_dir()?;
    Ok(profile_dir.join(format!("{profile_name}.yaml")))
}

fn default_profile_dir() -> Result<PathBuf, CliError> {
    let home = dirs::home_dir().ok_or_else(|| CliError::WithExit {
        message: "cannot determine home directory".to_string(),
        exit_code: ExitCode::InvalidInput,
    })?;
    Ok(home.join(DEFAULT_PROFILE_DIR_RELATIVE))
}

fn default_socket_path() -> Result<PathBuf, CliError> {
    let home = dirs::home_dir().ok_or_else(|| CliError::WithExit {
        message: "cannot determine home directory".to_string(),
        exit_code: ExitCode::InvalidInput,
    })?;
    Ok(home.join(DEFAULT_SOCKET_PATH_RELATIVE))
}

async fn daemon_client(timeout: Duration) -> Result<IpcClient, CliError> {
    let socket = default_socket_path()?;
    let client = IpcClient::new(socket.clone(), timeout);

    match client.send(DaemonRequest::Ping).await {
        Ok(_) => Ok(client),
        Err(_) => {
            ensure_daemon_running(&socket, timeout).await?;
            Ok(IpcClient::new(socket, timeout))
        }
    }
}

async fn ensure_daemon_running(socket: &Path, timeout: Duration) -> Result<(), CliError> {
    let ping_timeout = normalized_ping_timeout(timeout);
    let initial_ping_error = wait_for_daemon_ping(socket, ping_timeout, 5).await.err();
    if initial_ping_error.is_none() {
        return Ok(());
    }

    launch_daemon().map_err(|error| CliError::WithExit {
        message: error,
        exit_code: ExitCode::DaemonCommunication,
    })?;

    match wait_for_daemon_ping(socket, ping_timeout, DAEMON_PING_RETRY_COUNT).await {
        Ok(()) => Ok(()),
        Err(first_error) => {
            if socket.exists() && is_stale_socket_error(&first_error) {
                fs::remove_file(socket).map_err(|error| CliError::WithExit {
                    message: format!(
                        "failed to remove stale marsd socket {}: {error}",
                        socket.display()
                    ),
                    exit_code: ExitCode::DaemonCommunication,
                })?;

                launch_daemon().map_err(|error| CliError::WithExit {
                    message: error,
                    exit_code: ExitCode::DaemonCommunication,
                })?;

                wait_for_daemon_ping(socket, ping_timeout, DAEMON_PING_RETRY_COUNT)
                    .await
                    .map_err(|second_error| CliError::WithExit {
                        message: format!(
                            "failed to reach marsd after stale socket cleanup: {second_error}; initial ping error: {}",
                            initial_ping_error
                                .as_ref()
                                .map_or_else(|| "unknown".to_string(), std::string::ToString::to_string)
                        ),
                        exit_code: ExitCode::DaemonCommunication,
                    })?;
                return Ok(());
            }

            Err(CliError::WithExit {
                message: format!(
                    "failed to reach marsd after launch: {first_error}; initial ping error: {}",
                    initial_ping_error
                        .as_ref()
                        .map_or_else(|| "unknown".to_string(), std::string::ToString::to_string)
                ),
                exit_code: ExitCode::DaemonCommunication,
            })
        }
    }
}

fn validate_profile_name(profile_name: &str) -> Result<(), CliError> {
    if profile_name.trim().is_empty() {
        return Err(CliError::WithExit {
            message: "profile name cannot be empty".to_string(),
            exit_code: ExitCode::InvalidInput,
        });
    }

    if !is_valid_profile_name(profile_name) {
        return Err(CliError::WithExit {
            message: "invalid profile name: must match [a-zA-Z0-9][a-zA-Z0-9-_]{0,63}".to_string(),
            exit_code: ExitCode::InvalidInput,
        });
    }

    Ok(())
}

fn is_valid_profile_name(profile_name: &str) -> bool {
    if profile_name.is_empty() || profile_name.len() > PROFILE_NAME_MAX_LEN {
        return false;
    }

    let mut chars = profile_name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }

    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn normalized_ping_timeout(timeout: Duration) -> Duration {
    let max_ping_timeout = Duration::from_millis(200);
    if timeout.is_zero() {
        max_ping_timeout
    } else {
        timeout.min(max_ping_timeout)
    }
}

async fn wait_for_daemon_ping(
    socket: &Path,
    timeout: Duration,
    retries: usize,
) -> Result<(), mars_ipc::IpcError> {
    let mut last_error = None;

    for _ in 0..retries {
        let client = IpcClient::new(socket.to_path_buf(), timeout);
        match client.send(DaemonRequest::Ping).await {
            Ok(DaemonResponse::Pong) => return Ok(()),
            Ok(_) => {
                last_error = Some(mars_ipc::IpcError::DaemonError {
                    message: "unexpected daemon response to ping".to_string(),
                    exit_code: Some(ExitCode::DaemonCommunication),
                });
            }
            Err(error) => last_error = Some(error),
        }
        sleep(DAEMON_PING_RETRY_INTERVAL).await;
    }

    Err(last_error.unwrap_or(mars_ipc::IpcError::Timeout))
}

fn is_stale_socket_error(error: &mars_ipc::IpcError) -> bool {
    matches!(
        error,
        mars_ipc::IpcError::Io(io_error)
            if matches!(
                io_error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            )
    )
}

fn launch_daemon() -> Result<(), String> {
    let mut last_error = None::<String>;
    for launch in daemon_launch_candidates() {
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        if let Some(cwd) = launch.cwd.as_ref() {
            command.current_dir(cwd);
        }
        match command.spawn() {
            Ok(_child) => return Ok(()),
            Err(error) => {
                last_error = Some(format!(
                    "failed to start marsd with '{}': {error}",
                    launch.program
                ));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| "failed to start marsd".to_string()))
}

#[derive(Debug)]
struct LaunchCommand {
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

fn daemon_launch_candidates() -> Vec<LaunchCommand> {
    let mut candidates = Vec::new();

    if let Ok(path) = std::env::var("MARS_DAEMON_BIN") {
        candidates.push(LaunchCommand {
            program: path,
            args: vec!["--serve".to_string()],
            cwd: None,
        });
    }

    if let Ok(current) = std::env::current_exe() {
        let sibling = current.with_file_name("marsd");
        if sibling.exists() {
            candidates.push(LaunchCommand {
                program: sibling.display().to_string(),
                args: vec!["--serve".to_string()],
                cwd: None,
            });
        }
    }

    if Path::new("/usr/local/bin/marsd").exists() {
        candidates.push(LaunchCommand {
            program: "/usr/local/bin/marsd".to_string(),
            args: vec!["--serve".to_string()],
            cwd: None,
        });
    }

    candidates.push(LaunchCommand {
        program: "marsd".to_string(),
        args: vec!["--serve".to_string()],
        cwd: None,
    });

    if Path::new("Cargo.toml").exists() {
        candidates.push(LaunchCommand {
            program: "cargo".to_string(),
            args: vec![
                "run".to_string(),
                "-p".to_string(),
                "mars-daemon".to_string(),
                "--bin".to_string(),
                "marsd".to_string(),
                "--".to_string(),
                "--serve".to_string(),
            ],
            cwd: Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        });
    }

    candidates
}

fn ipc_to_cli_error(error: mars_ipc::IpcError) -> CliError {
    match error {
        mars_ipc::IpcError::DaemonError { message, exit_code } => CliError::WithExit {
            message,
            exit_code: exit_code.unwrap_or(ExitCode::DaemonCommunication),
        },
        other => CliError::WithExit {
            message: other.to_string(),
            exit_code: ExitCode::DaemonCommunication,
        },
    }
}

async fn request_logs(
    client: &IpcClient,
    follow: bool,
    cursor: Option<u64>,
    limit: Option<u32>,
) -> Result<LogResponse, CliError> {
    let response = client
        .send(DaemonRequest::Logs(LogRequest {
            follow,
            cursor,
            limit,
        }))
        .await
        .map_err(ipc_to_cli_error)?;
    let DaemonResponse::Logs(result) = response else {
        return Err(CliError::WithExit {
            message: "unexpected daemon response for logs".to_string(),
            exit_code: ExitCode::DaemonCommunication,
        });
    };
    Ok(result)
}

async fn follow_logs(client: &IpcClient) -> Result<ExitCode, CliError> {
    let initial = request_logs(client, true, None, Some(200)).await?;
    for line in initial.lines {
        println!("{line}");
    }
    let mut cursor = initial.next_cursor;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(ExitCode::Interrupted),
            _ = sleep(LOG_FOLLOW_POLL_INTERVAL) => {}
        }

        let snapshot = request_logs(client, true, Some(cursor), None).await?;
        for line in snapshot.lines {
            println!("{line}");
        }
        cursor = snapshot.next_cursor;
    }
}

#[cfg(test)]
fn compute_log_delta(previous: &[String], current: &[String]) -> Vec<String> {
    if previous.is_empty() || current.len() < previous.len() {
        return current.to_vec();
    }

    let max_overlap = previous.len().min(current.len());
    for overlap in (0..=max_overlap).rev() {
        if previous[previous.len() - overlap..] == current[..overlap] {
            return current[overlap..].to_vec();
        }
    }

    current.to_vec()
}

#[cfg(test)]
mod tests {
    use mars_types::ExternalDeviceInfo;

    use super::{
        InternalLatencyTestReport, ProbeReport, TEST_MIC_ID, TEST_SPEAKER_ID,
        TEST_VIRTUAL_INPUT_ID, TEST_VIRTUAL_INPUT_UID, TEST_VIRTUAL_OUTPUT_ID,
        TEST_VIRTUAL_OUTPUT_UID, build_internal_test_profile, build_route_test_profile,
        compute_log_delta, coreaudio_uid, format_internal_latency_test_report,
        internal_latency_test_context, is_stale_socket_error, is_valid_profile_name, probe_report,
    };

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

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
    fn accepts_valid_profile_names() {
        assert!(is_valid_profile_name("demo"));
        assert!(is_valid_profile_name("demo-1"));
        assert!(is_valid_profile_name("A_1"));
    }

    #[test]
    fn rejects_invalid_profile_names() {
        assert!(!is_valid_profile_name("../x"));
        assert!(!is_valid_profile_name("a/b"));
        assert!(!is_valid_profile_name(".."));
        assert!(!is_valid_profile_name(""));
        assert!(!is_valid_profile_name(" demo"));
    }

    #[test]
    fn stale_socket_detection_covers_connect_errors_only() {
        let not_found =
            mars_ipc::IpcError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        assert!(is_stale_socket_error(&not_found));

        let refused = mars_ipc::IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(is_stale_socket_error(&refused));

        let timed_out = mars_ipc::IpcError::Timeout;
        assert!(!is_stale_socket_error(&timed_out));
    }

    #[test]
    fn log_delta_returns_all_lines_on_first_snapshot() {
        let current = lines(&["one", "two"]);
        assert_eq!(compute_log_delta(&[], &current), current);
    }

    #[test]
    fn log_delta_returns_only_new_lines_on_append() {
        let previous = lines(&["one", "two"]);
        let current = lines(&["one", "two", "three", "four"]);
        assert_eq!(
            compute_log_delta(&previous, &current),
            lines(&["three", "four"])
        );
    }

    #[test]
    fn log_delta_returns_full_snapshot_on_truncation() {
        let previous = lines(&["one", "two", "three", "four"]);
        let current = lines(&["new-one", "new-two"]);
        assert_eq!(compute_log_delta(&previous, &current), current);
    }

    #[test]
    fn log_delta_returns_no_lines_when_snapshot_unchanged() {
        let previous = lines(&["one", "two"]);
        let current = lines(&["one", "two"]);
        assert!(compute_log_delta(&previous, &current).is_empty());
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

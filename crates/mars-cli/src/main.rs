#![forbid(unsafe_code)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::{Parser, Subcommand};
use mars_ipc::{DaemonRequest, DaemonResponse, IpcClient, LogRequest, LogResponse};
use mars_profile::{TemplateKind, render_template};
use mars_types::{
    ApplyRequest, ClearRequest, DEFAULT_PROFILE_DIR_RELATIVE, DEFAULT_SOCKET_PATH_RELATIVE,
    ExitCode, PlanRequest, ValidateRequest,
};
use serde::Serialize;
use thiserror::Error;
use tokio::time::sleep;

mod test_command;

const PROFILE_NAME_MAX_LEN: usize = 64;
const DAEMON_PING_RETRY_COUNT: usize = 50;
const DAEMON_PING_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const LOG_FOLLOW_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
            let client = daemon_client(Duration::from_millis(5_000)).await?;
            if route {
                if !cli.json {
                    println!(
                        "Speak into the microphone or clap near it for the next {} seconds...",
                        test_command::ROUTE_LISTEN_DURATION.as_secs()
                    );
                }
                let report = test_command::run_route_test_command(
                    &client,
                    mic.as_deref(),
                    speaker.as_deref(),
                    sample_rate,
                    buffer_frames,
                )
                .await?;
                print_output(cli.json, &report, || {
                    test_command::format_route_test_report(&report)
                })?;
            } else {
                if mic.is_some() || speaker.is_some() {
                    return Err(CliError::WithExit {
                        message: "`--mic` and `--speaker` require `mars test --route`".to_string(),
                        exit_code: ExitCode::InvalidInput,
                    });
                }
                let report = test_command::run_internal_latency_test_command(
                    &client,
                    sample_rate,
                    buffer_frames,
                )
                .await?;
                print_output(cli.json, &report, || {
                    test_command::format_internal_latency_test_report(&report)
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
    use super::{compute_log_delta, is_stale_socket_error, is_valid_profile_name};

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
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
}

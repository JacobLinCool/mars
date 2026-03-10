#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use mars_types::{
    AuPluginConfig, PLUGIN_HOST_PROTOCOL_VERSION, PluginHostRequest, PluginHostResponse,
};

#[derive(Debug, Clone)]
struct LoadedInstance {
    config: AuPluginConfig,
    prepared: bool,
    sample_rate: u32,
    channels: u16,
    max_frames: u32,
}

fn parse_socket_path() -> Result<PathBuf, String> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" {
            let Some(path) = args.next() else {
                return Err("missing path after --socket".to_string());
            };
            return Ok(PathBuf::from(path));
        }
    }
    Err("missing required --socket argument".to_string())
}

fn send_response(stream: &mut UnixStream, response: &PluginHostResponse) -> Result<(), String> {
    let mut payload = serde_json::to_string(response)
        .map_err(|error| format!("failed to serialize response: {error}"))?;
    payload.push('\n');
    stream
        .write_all(payload.as_bytes())
        .map_err(|error| format!("failed to write response: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("failed to flush response: {error}"))
}

fn error_response(message: impl Into<String>) -> PluginHostResponse {
    PluginHostResponse::Error {
        message: message.into(),
    }
}

fn handle_request(
    request: PluginHostRequest,
    instances: &mut BTreeMap<String, LoadedInstance>,
    process_count: &mut u64,
    crash_after_process: Option<u64>,
    process_delay: Duration,
) -> PluginHostResponse {
    match request {
        PluginHostRequest::Handshake { protocol_version } => {
            if protocol_version != PLUGIN_HOST_PROTOCOL_VERSION {
                return error_response(format!(
                    "protocol version mismatch: expected {PLUGIN_HOST_PROTOCOL_VERSION}, got {protocol_version}"
                ));
            }
            PluginHostResponse::Handshake {
                protocol_version: PLUGIN_HOST_PROTOCOL_VERSION,
            }
        }
        PluginHostRequest::Load {
            instance_id,
            config,
        } => {
            instances.insert(
                instance_id,
                LoadedInstance {
                    config,
                    prepared: false,
                    sample_rate: 0,
                    channels: 0,
                    max_frames: 0,
                },
            );
            PluginHostResponse::Ack
        }
        PluginHostRequest::Prepare {
            instance_id,
            sample_rate,
            channels,
            max_frames,
        } => {
            let Some(instance) = instances.get_mut(&instance_id) else {
                return error_response(format!("instance '{instance_id}' is not loaded"));
            };
            if channels == 0 {
                return error_response("channels must be > 0".to_string());
            }
            if max_frames == 0 {
                return error_response("max_frames must be > 0".to_string());
            }
            if max_frames > instance.config.max_frames {
                return error_response(format!(
                    "max_frames exceeds configured limit: requested={max_frames}, configured={}",
                    instance.config.max_frames
                ));
            }
            instance.prepared = true;
            instance.sample_rate = sample_rate;
            instance.channels = channels;
            instance.max_frames = max_frames;
            PluginHostResponse::Ack
        }
        PluginHostRequest::Process {
            instance_id,
            channels,
            frames,
            samples,
        } => {
            let Some(instance) = instances.get(&instance_id) else {
                return error_response(format!("instance '{instance_id}' is not loaded"));
            };
            if !instance.prepared {
                return error_response(format!("instance '{instance_id}' is not prepared"));
            }
            if channels != instance.channels {
                return error_response(format!(
                    "channel mismatch for '{instance_id}': expected {}, got {channels}",
                    instance.channels
                ));
            }
            if frames > instance.max_frames {
                return error_response(format!(
                    "frame count exceeds prepared max_frames: frames={frames}, max_frames={}",
                    instance.max_frames
                ));
            }
            let expected = (channels as usize).saturating_mul(frames as usize);
            if samples.len() != expected {
                return error_response(format!(
                    "sample count mismatch: expected {expected}, got {}",
                    samples.len()
                ));
            }

            if !process_delay.is_zero() {
                thread::sleep(process_delay);
            }

            *process_count = process_count.saturating_add(1);
            if let Some(crash_after) = crash_after_process {
                if *process_count >= crash_after {
                    std::process::exit(77);
                }
            }

            PluginHostResponse::Processed { samples }
        }
        PluginHostRequest::Reset { instance_id } => {
            let Some(instance) = instances.get_mut(&instance_id) else {
                return error_response(format!("instance '{instance_id}' is not loaded"));
            };
            instance.prepared = false;
            PluginHostResponse::Ack
        }
        PluginHostRequest::Unload { instance_id } => {
            instances.remove(&instance_id);
            PluginHostResponse::Ack
        }
        PluginHostRequest::Shutdown => PluginHostResponse::Ack,
    }
}

fn run_server(socket_path: PathBuf) -> Result<(), String> {
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| format!("failed to bind {}: {error}", socket_path.display()))?;

    let crash_after_process = env::var("MARS_PLUGIN_HOST_CRASH_AFTER_PROCESS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    let process_delay = env::var("MARS_PLUGIN_HOST_PROCESS_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_default();

    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("failed to accept plugin host connection: {error}"))?;
    let reader_stream = stream
        .try_clone()
        .map_err(|error| format!("failed to clone plugin host stream: {error}"))?;
    let mut reader = BufReader::new(reader_stream);

    let mut instances = BTreeMap::<String, LoadedInstance>::new();
    let mut process_count = 0u64;
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read plugin host request: {error}"))?;
        if bytes == 0 {
            break;
        }

        let request = match serde_json::from_str::<PluginHostRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                send_response(
                    &mut stream,
                    &error_response(format!("invalid plugin host request: {error}")),
                )?;
                continue;
            }
        };

        let shutdown = matches!(request, PluginHostRequest::Shutdown);
        let response = handle_request(
            request,
            &mut instances,
            &mut process_count,
            crash_after_process,
            process_delay,
        );
        send_response(&mut stream, &response)?;

        if shutdown {
            break;
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

fn main() {
    let exit = match parse_socket_path().and_then(run_server) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    };
    std::process::exit(exit);
}

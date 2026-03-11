#![forbid(unsafe_code)]
//! JSONL IPC protocol between `mars` and `marsd`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use mars_telemetry::{Attribute, TelemetryTracer, U64Counter, U64Histogram};
use mars_types::{
    ApplyPlan, ApplyRequest, ApplyResult, CaptureProcessInfo, ClearRequest, DaemonStatus,
    DeviceInventory, DoctorReport, ExitCode, PlanRequest, ValidateRequest, ValidationReport,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;
use tracing::{debug, warn};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 2;

#[derive(Debug)]
struct IpcTelemetry {
    tracer: TelemetryTracer,
    client_request_count: U64Counter,
    client_request_duration: U64Histogram,
    daemon_request_count: U64Counter,
    daemon_request_duration: U64Histogram,
}

static IPC_TELEMETRY: OnceLock<IpcTelemetry> = OnceLock::new();

fn ipc_telemetry() -> &'static IpcTelemetry {
    IPC_TELEMETRY.get_or_init(|| {
        let meter = mars_telemetry::global_meter("mars-ipc");
        IpcTelemetry {
            tracer: mars_telemetry::global_tracer("mars-ipc"),
            client_request_count: meter.u64_counter(
                "mars.ipc.request.count",
                "Count of IPC client requests",
                "{request}",
            ),
            client_request_duration: meter.u64_histogram(
                "mars.ipc.request.duration",
                "Duration of IPC client requests",
                "ms",
            ),
            daemon_request_count: meter.u64_counter(
                "mars.daemon.request.count",
                "Count of daemon requests",
                "{request}",
            ),
            daemon_request_duration: meter.u64_histogram(
                "mars.daemon.request.duration",
                "Duration of daemon requests",
                "ms",
            ),
        }
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Ping,
    Validate,
    Plan,
    Apply,
    Clear,
    Status,
    Devices,
    Processes,
    Logs,
    Doctor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogRequest {
    pub follow: bool,
    #[serde(default)]
    pub cursor: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogResponse {
    pub lines: Vec<String>,
    pub next_cursor: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestEnvelope {
    pub protocol_version: u16,
    pub request_id: String,
    pub command: Command,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseEnvelope {
    pub protocol_version: u16,
    pub request_id: String,
    pub command: Command,
    pub ok: bool,
    pub payload: Value,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub enum DaemonRequest {
    Ping,
    Validate(ValidateRequest),
    Plan(PlanRequest),
    Apply(ApplyRequest),
    Clear(ClearRequest),
    Status,
    Devices,
    Processes,
    Logs(LogRequest),
    Doctor,
}

impl DaemonRequest {
    #[must_use]
    pub fn command(&self) -> Command {
        match self {
            Self::Ping => Command::Ping,
            Self::Validate(_) => Command::Validate,
            Self::Plan(_) => Command::Plan,
            Self::Apply(_) => Command::Apply,
            Self::Clear(_) => Command::Clear,
            Self::Status => Command::Status,
            Self::Devices => Command::Devices,
            Self::Processes => Command::Processes,
            Self::Logs(_) => Command::Logs,
            Self::Doctor => Command::Doctor,
        }
    }

    pub fn into_payload(self) -> Result<Value, IpcError> {
        match self {
            Self::Ping | Self::Status | Self::Devices | Self::Processes | Self::Doctor => {
                Ok(Value::Null)
            }
            Self::Validate(payload) => serde_json::to_value(payload).map_err(IpcError::SerdeJson),
            Self::Plan(payload) => serde_json::to_value(payload).map_err(IpcError::SerdeJson),
            Self::Apply(payload) => serde_json::to_value(payload).map_err(IpcError::SerdeJson),
            Self::Clear(payload) => serde_json::to_value(payload).map_err(IpcError::SerdeJson),
            Self::Logs(payload) => serde_json::to_value(payload).map_err(IpcError::SerdeJson),
        }
    }
}

#[derive(Debug, Clone)]
pub enum DaemonResponse {
    Pong,
    Validate(ValidationReport),
    Plan(ApplyPlan),
    Apply(ApplyResult),
    Clear(ApplyResult),
    Status(DaemonStatus),
    Devices(DeviceInventory),
    Processes(Vec<CaptureProcessInfo>),
    Logs(LogResponse),
    Doctor(DoctorReport),
}

impl DaemonResponse {
    pub fn from_payload(command: Command, payload: Value) -> Result<Self, IpcError> {
        match command {
            Command::Ping => Ok(Self::Pong),
            Command::Validate => Ok(Self::Validate(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
            Command::Plan => Ok(Self::Plan(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
            Command::Apply => Ok(Self::Apply(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
            Command::Clear => Ok(Self::Clear(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
            Command::Status => Ok(Self::Status(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
            Command::Devices => Ok(Self::Devices(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
            Command::Processes => Ok(Self::Processes(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
            Command::Logs => Ok(Self::Logs(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
            Command::Doctor => Ok(Self::Doctor(
                serde_json::from_value(payload).map_err(IpcError::SerdeJson)?,
            )),
        }
    }

    pub fn into_payload(self) -> Result<Value, IpcError> {
        match self {
            Self::Pong => Ok(Value::Null),
            Self::Validate(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
            Self::Plan(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
            Self::Apply(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
            Self::Clear(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
            Self::Status(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
            Self::Devices(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
            Self::Processes(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
            Self::Logs(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
            Self::Doctor(value) => serde_json::to_value(value).map_err(IpcError::SerdeJson),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub message: String,
    pub exit_code: ExitCode,
}

impl ApiError {
    #[must_use]
    pub fn new(message: impl Into<String>, exit_code: ExitCode) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    SerdeJson(serde_json::Error),
    #[error("timeout while waiting for daemon response")]
    Timeout,
    #[error("protocol mismatch: expected {expected}, got {actual}")]
    ProtocolVersionMismatch { expected: u16, actual: u16 },
    #[error("daemon returned error: {message}")]
    DaemonError {
        message: String,
        exit_code: Option<ExitCode>,
    },
    #[error("invalid request payload for command {0:?}: {1}")]
    InvalidRequestPayload(Command, serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct IpcClient {
    socket_path: PathBuf,
    timeout: Duration,
}

impl IpcClient {
    #[must_use]
    pub fn new(socket_path: PathBuf, timeout: Duration) -> Self {
        Self {
            socket_path,
            timeout,
        }
    }

    pub async fn send(&self, request: DaemonRequest) -> Result<DaemonResponse, IpcError> {
        let command = request.command();
        let command_name = command_label(command);
        let payload = request.into_payload()?;
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4().to_string(),
            command,
            payload,
        };
        let request_id = envelope.request_id.clone();
        let timeout_ms = self.timeout.as_millis().min(u128::from(u64::MAX)) as u64;
        let telemetry = ipc_telemetry();
        let mut span = telemetry.tracer.start_span(
            "mars.ipc.client.request",
            &[
                Attribute::string("command", command_name),
                Attribute::string("request_id", request_id.clone()),
                Attribute::u64("timeout_ms", timeout_ms),
            ],
        );
        let started = Instant::now();

        let outcome: Result<DaemonResponse, IpcError> = async {
            let stream = UnixStream::connect(&self.socket_path).await?;
            let (reader, mut writer) = stream.into_split();
            let encoded = serde_json::to_string(&envelope).map_err(IpcError::SerdeJson)?;
            writer.write_all(encoded.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;

            let mut lines = BufReader::new(reader).lines();
            let maybe_line = timeout(self.timeout, lines.next_line())
                .await
                .map_err(|_| IpcError::Timeout)??;
            let Some(line) = maybe_line else {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "daemon closed connection",
                )));
            };

            let response: ResponseEnvelope =
                serde_json::from_str(&line).map_err(IpcError::SerdeJson)?;
            if response.protocol_version != PROTOCOL_VERSION {
                return Err(IpcError::ProtocolVersionMismatch {
                    expected: PROTOCOL_VERSION,
                    actual: response.protocol_version,
                });
            }

            if !response.ok {
                return Err(IpcError::DaemonError {
                    message: response
                        .error
                        .unwrap_or_else(|| "unknown daemon error".to_string()),
                    exit_code: response
                        .exit_code
                        .and_then(|code| ExitCode::try_from(code).ok()),
                });
            }

            DaemonResponse::from_payload(response.command, response.payload)
        }
        .await;

        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let success = outcome.is_ok();
        let attrs = [
            Attribute::string("command", command_name),
            Attribute::bool("success", success),
        ];
        telemetry.client_request_count.add(1, &attrs);
        telemetry.client_request_duration.record(elapsed_ms, &attrs);
        span.set_attributes(&attrs);
        if let Err(error) = outcome.as_ref() {
            span.set_status_error(error.to_string());
        } else {
            span.set_status_ok();
        }
        span.end();

        outcome
    }
}

#[async_trait]
pub trait RequestHandler: Send + Sync + 'static {
    async fn handle(&self, request: DaemonRequest) -> Result<DaemonResponse, ApiError>;
}

pub async fn serve<H>(socket_path: &Path, handler: Arc<H>) -> Result<(), IpcError>
where
    H: RequestHandler,
{
    if socket_path.exists() {
        let _ = tokio::fs::remove_file(socket_path).await;
    }

    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let listener = UnixListener::bind(socket_path)?;
    debug!(?socket_path, "marsd ipc listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, handler).await {
                warn!(error = %error, "failed to process ipc connection");
            }
        });
    }
}

async fn handle_connection<H>(stream: UnixStream, handler: Arc<H>) -> Result<(), IpcError>
where
    H: RequestHandler,
{
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let telemetry = ipc_telemetry();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let envelope: RequestEnvelope = serde_json::from_str(&line).map_err(IpcError::SerdeJson)?;
        let command_name = command_label(envelope.command);
        let request_id = envelope.request_id.clone();
        let mut span = telemetry.tracer.start_span(
            "mars.daemon.request",
            &[
                Attribute::string("command", command_name),
                Attribute::string("request_id", request_id),
            ],
        );
        let started = Instant::now();

        let response = if envelope.protocol_version != PROTOCOL_VERSION {
            ResponseEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request_id: envelope.request_id,
                command: envelope.command,
                ok: false,
                payload: Value::Null,
                error: Some(format!(
                    "protocol mismatch: daemon={} client={}",
                    PROTOCOL_VERSION, envelope.protocol_version
                )),
                exit_code: Some(ExitCode::DaemonCommunication.as_i32()),
            }
        } else {
            let request = deserialize_request(envelope.command, envelope.payload);
            match request {
                Ok(request) => match handler.handle(request).await {
                    Ok(payload) => ResponseEnvelope {
                        protocol_version: PROTOCOL_VERSION,
                        request_id: envelope.request_id,
                        command: envelope.command,
                        ok: true,
                        payload: payload.into_payload()?,
                        error: None,
                        exit_code: None,
                    },
                    Err(error) => ResponseEnvelope {
                        protocol_version: PROTOCOL_VERSION,
                        request_id: envelope.request_id,
                        command: envelope.command,
                        ok: false,
                        payload: Value::Null,
                        error: Some(error.message),
                        exit_code: Some(error.exit_code.as_i32()),
                    },
                },
                Err(error) => ResponseEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    request_id: envelope.request_id,
                    command: envelope.command,
                    ok: false,
                    payload: Value::Null,
                    error: Some(error.to_string()),
                    exit_code: Some(ExitCode::InvalidInput.as_i32()),
                },
            }
        };

        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let attrs = [
            Attribute::string("command", command_name),
            Attribute::bool("success", response.ok),
            Attribute::i64("exit_code", i64::from(response.exit_code.unwrap_or(0))),
        ];
        telemetry.daemon_request_count.add(1, &attrs);
        telemetry.daemon_request_duration.record(elapsed_ms, &attrs);
        span.set_attributes(&attrs);
        if response.ok {
            span.set_status_ok();
        } else {
            span.set_status_error(
                response
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown daemon error".to_string()),
            );
        }
        span.end();
        write_response(&mut writer, &response).await?;
    }

    Ok(())
}

fn deserialize_request(command: Command, payload: Value) -> Result<DaemonRequest, IpcError> {
    match command {
        Command::Ping => Ok(DaemonRequest::Ping),
        Command::Validate => serde_json::from_value(payload)
            .map(DaemonRequest::Validate)
            .map_err(|error| IpcError::InvalidRequestPayload(command, error)),
        Command::Plan => serde_json::from_value(payload)
            .map(DaemonRequest::Plan)
            .map_err(|error| IpcError::InvalidRequestPayload(command, error)),
        Command::Apply => serde_json::from_value(payload)
            .map(DaemonRequest::Apply)
            .map_err(|error| IpcError::InvalidRequestPayload(command, error)),
        Command::Clear => serde_json::from_value(payload)
            .map(DaemonRequest::Clear)
            .map_err(|error| IpcError::InvalidRequestPayload(command, error)),
        Command::Status => Ok(DaemonRequest::Status),
        Command::Devices => Ok(DaemonRequest::Devices),
        Command::Processes => Ok(DaemonRequest::Processes),
        Command::Logs => serde_json::from_value(payload)
            .map(DaemonRequest::Logs)
            .map_err(|error| IpcError::InvalidRequestPayload(command, error)),
        Command::Doctor => Ok(DaemonRequest::Doctor),
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &ResponseEnvelope,
) -> Result<(), IpcError> {
    let encoded = serde_json::to_string(response).map_err(IpcError::SerdeJson)?;
    writer.write_all(encoded.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

const fn command_label(command: Command) -> &'static str {
    match command {
        Command::Ping => "ping",
        Command::Validate => "validate",
        Command::Plan => "plan",
        Command::Apply => "apply",
        Command::Clear => "clear",
        Command::Status => "status",
        Command::Devices => "devices",
        Command::Processes => "processes",
        Command::Logs => "logs",
        Command::Doctor => "doctor",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use super::*;

    #[derive(Debug, Clone, Copy)]
    enum HandlerMode {
        Success,
        Error,
    }

    #[derive(Debug, Clone)]
    struct TestHandler {
        calls: Arc<AtomicUsize>,
        mode: HandlerMode,
    }

    #[async_trait]
    impl RequestHandler for TestHandler {
        async fn handle(&self, _request: DaemonRequest) -> Result<DaemonResponse, ApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                HandlerMode::Success => Ok(DaemonResponse::Pong),
                HandlerMode::Error => Err(ApiError::new("boom", ExitCode::ApplyFailed)),
            }
        }
    }

    fn request_envelope(
        protocol_version: u16,
        command: Command,
        payload: Value,
    ) -> RequestEnvelope {
        RequestEnvelope {
            protocol_version,
            request_id: "test-request-id".to_string(),
            command,
            payload,
        }
    }

    async fn invoke_handler<H>(request: RequestEnvelope, handler: Arc<H>) -> ResponseEnvelope
    where
        H: RequestHandler,
    {
        let (client, server) = UnixStream::pair().expect("create unix stream pair");
        let server_task = tokio::spawn(async move { handle_connection(server, handler).await });

        let (reader, mut writer) = client.into_split();
        let encoded = serde_json::to_string(&request).expect("serialize request");
        writer
            .write_all(encoded.as_bytes())
            .await
            .expect("write request");
        writer.write_all(b"\n").await.expect("write newline");
        writer.shutdown().await.expect("shutdown writer");

        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await
            .expect("read response")
            .expect("response line");
        let response: ResponseEnvelope = serde_json::from_str(&line).expect("deserialize response");

        let result = server_task.await.expect("join server task");
        assert!(result.is_ok(), "connection handler should exit cleanly");

        response
    }

    #[tokio::test]
    async fn protocol_mismatch_returns_daemon_communication_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(TestHandler {
            calls: Arc::clone(&calls),
            mode: HandlerMode::Success,
        });
        let request = request_envelope(PROTOCOL_VERSION + 1, Command::Ping, Value::Null);

        let response = invoke_handler(request, handler).await;

        assert!(!response.ok);
        assert_eq!(
            response.exit_code,
            Some(ExitCode::DaemonCommunication.as_i32())
        );
        assert!(
            response
                .error
                .expect("protocol mismatch error")
                .contains("protocol mismatch")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn invalid_payload_returns_invalid_input_error_without_calling_handler() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(TestHandler {
            calls: Arc::clone(&calls),
            mode: HandlerMode::Success,
        });
        let request = request_envelope(PROTOCOL_VERSION, Command::Logs, Value::Null);

        let response = invoke_handler(request, handler).await;

        assert!(!response.ok);
        assert_eq!(response.command, Command::Logs);
        assert_eq!(response.exit_code, Some(ExitCode::InvalidInput.as_i32()));
        assert!(
            response
                .error
                .expect("invalid payload error")
                .contains("invalid request payload")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn handler_error_is_returned_with_exit_code() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(TestHandler {
            calls: Arc::clone(&calls),
            mode: HandlerMode::Error,
        });
        let request = request_envelope(PROTOCOL_VERSION, Command::Ping, Value::Null);

        let response = invoke_handler(request, handler).await;

        assert!(!response.ok);
        assert_eq!(response.error.as_deref(), Some("boom"));
        assert_eq!(response.exit_code, Some(ExitCode::ApplyFailed.as_i32()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn deserialize_logs_request_reads_optional_fields() {
        let request = deserialize_request(
            Command::Logs,
            json!({
                "follow": true,
                "cursor": 42,
                "limit": 7
            }),
        )
        .expect("parse logs payload");

        assert!(matches!(
            request,
            DaemonRequest::Logs(LogRequest {
                follow: true,
                cursor: Some(42),
                limit: Some(7)
            })
        ));
    }

    #[test]
    fn deserialize_processes_request_accepts_null_payload() {
        let request =
            deserialize_request(Command::Processes, Value::Null).expect("parse processes payload");
        assert!(matches!(request, DaemonRequest::Processes));
    }

    #[test]
    fn processes_response_payload_round_trip_preserves_fields() {
        let payload = DaemonResponse::Processes(vec![CaptureProcessInfo {
            process_object_id: 42,
            pid: 9001,
            bundle_id: "com.example.App".to_string(),
            is_running: true,
            is_running_input: true,
            is_running_output: false,
        }])
        .into_payload()
        .expect("serialize processes response");

        let DaemonResponse::Processes(processes) =
            DaemonResponse::from_payload(Command::Processes, payload)
                .expect("deserialize processes response")
        else {
            panic!("expected processes response");
        };

        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].process_object_id, 42);
        assert_eq!(processes[0].pid, 9001);
        assert_eq!(processes[0].bundle_id, "com.example.App");
        assert!(processes[0].is_running);
        assert!(processes[0].is_running_input);
        assert!(!processes[0].is_running_output);
    }
}

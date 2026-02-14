#![forbid(unsafe_code)]
//! JSONL IPC protocol between `mars` and `marsd`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mars_types::{
    ApplyPlan, ApplyRequest, ApplyResult, ClearRequest, DaemonStatus, DeviceInventory,
    DoctorReport, ExitCode, PlanRequest, ValidateRequest, ValidationReport,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;
use tracing::{debug, warn};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;

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
    Logs,
    Doctor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogRequest {
    pub follow: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogResponse {
    pub lines: Vec<String>,
    pub streaming: bool,
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
            Self::Logs(_) => Command::Logs,
            Self::Doctor => Command::Doctor,
        }
    }

    pub fn into_payload(self) -> Result<Value, IpcError> {
        match self {
            Self::Ping | Self::Status | Self::Devices | Self::Doctor => Ok(Value::Null),
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
        let payload = request.into_payload()?;
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4().to_string(),
            command,
            payload,
        };

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

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let envelope: RequestEnvelope = serde_json::from_str(&line).map_err(IpcError::SerdeJson)?;
        if envelope.protocol_version != PROTOCOL_VERSION {
            let response = ResponseEnvelope {
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
            };
            write_response(&mut writer, &response).await?;
            continue;
        }

        let request = deserialize_request(envelope.command, envelope.payload);
        let response = match request {
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
        };

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

#![forbid(unsafe_code)]

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, channel, sync_channel,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mars_types::{
    AuPluginConfig, PLUGIN_HOST_PROTOCOL_VERSION, PluginHostHealth, PluginHostInstanceStatus,
    PluginHostRequest, PluginHostResponse,
};
use parking_lot::Mutex;

const RT_QUEUE_CAPACITY: usize = 2;
const RESULT_QUEUE_CAPACITY: usize = 1;
const WORKER_POLL_INTERVAL_MS: u64 = 5;
const HOST_STARTUP_TIMEOUT_MS: u64 = 2_000;
const HOST_CONNECT_RETRY_MS: u64 = 20;

#[derive(Debug)]
pub struct AuWorker {
    request_tx: SyncSender<AuProcessRequest>,
    result_rx: Receiver<Vec<f32>>,
    stop_tx: std::sync::mpsc::Sender<()>,
    status: Arc<Mutex<PluginHostInstanceStatus>>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub struct AuWorkerSettings {
    pub processor_id: String,
    pub config: AuPluginConfig,
    pub sample_rate: u32,
    pub channels: usize,
    pub max_frames: usize,
}

#[derive(Debug, Clone)]
pub struct AuProcessRequest {
    pub frames: usize,
    pub channels: usize,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuSubmitError {
    Full,
    Disconnected,
}

#[derive(Debug)]
struct HostSession {
    child: Child,
    socket_path: PathBuf,
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    instance_id: String,
    config: AuPluginConfig,
}

#[derive(Debug, Clone)]
enum HostSessionError {
    Timeout(String),
    Io(String),
}

impl HostSessionError {
    fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }

    fn message(&self) -> String {
        match self {
            Self::Timeout(message) | Self::Io(message) => message.clone(),
        }
    }
}

impl AuWorker {
    pub fn start(settings: AuWorkerSettings) -> Self {
        let (request_tx, request_rx) = sync_channel::<AuProcessRequest>(RT_QUEUE_CAPACITY);
        let (result_tx, result_rx) = sync_channel::<Vec<f32>>(RESULT_QUEUE_CAPACITY);
        let (stop_tx, stop_rx) = channel::<()>();
        let status = Arc::new(Mutex::new(PluginHostInstanceStatus {
            id: settings.processor_id.clone(),
            api: settings.config.api,
            health: PluginHostHealth::Degraded,
            loaded: false,
            host_pid: None,
            process_calls: 0,
            timeout_count: 0,
            error_count: 0,
            restart_count: 0,
            last_error: None,
        }));

        let thread_status = status.clone();
        let handle = thread::Builder::new()
            .name(format!("marsd-au-{}", settings.processor_id))
            .spawn(move || {
                run_worker(settings, request_rx, result_tx, stop_rx, thread_status);
            })
            .ok();

        Self {
            request_tx,
            result_rx,
            stop_tx,
            status,
            handle,
        }
    }

    pub fn try_submit(&self, request: AuProcessRequest) -> Result<(), AuSubmitError> {
        match self.request_tx.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_request)) => {
                let mut status = self.status.lock();
                status.timeout_count = status.timeout_count.saturating_add(1);
                status.health = PluginHostHealth::Degraded;
                status.last_error = Some("plugin host rt queue full".to_string());
                Err(AuSubmitError::Full)
            }
            Err(TrySendError::Disconnected(_request)) => {
                let mut status = self.status.lock();
                status.error_count = status.error_count.saturating_add(1);
                status.health = PluginHostHealth::Failed;
                status.loaded = false;
                status.host_pid = None;
                status.last_error = Some("plugin host worker disconnected".to_string());
                Err(AuSubmitError::Disconnected)
            }
        }
    }

    pub fn drain_latest_result(&self) -> Option<Vec<f32>> {
        let mut latest = None;
        loop {
            match self.result_rx.try_recv() {
                Ok(result) => latest = Some(result),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        latest
    }

    pub fn status_snapshot(&self) -> PluginHostInstanceStatus {
        self.status.lock().clone()
    }

    pub fn stop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AuWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_worker(
    settings: AuWorkerSettings,
    request_rx: Receiver<AuProcessRequest>,
    result_tx: SyncSender<Vec<f32>>,
    stop_rx: std::sync::mpsc::Receiver<()>,
    status: Arc<Mutex<PluginHostInstanceStatus>>,
) {
    let mut session = match HostSession::start(&settings) {
        Ok(session) => {
            {
                let mut lock = status.lock();
                lock.loaded = true;
                lock.health = PluginHostHealth::Healthy;
                lock.host_pid = Some(session.pid());
                lock.last_error = None;
            }
            Some(session)
        }
        Err(error) => {
            let mut lock = status.lock();
            lock.loaded = false;
            lock.host_pid = None;
            lock.health = PluginHostHealth::Failed;
            lock.error_count = lock.error_count.saturating_add(1);
            lock.last_error = Some(error.message());
            None
        }
    };

    let poll = Duration::from_millis(WORKER_POLL_INTERVAL_MS);
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let request = match request_rx.recv_timeout(poll) {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        {
            let mut lock = status.lock();
            lock.process_calls = lock.process_calls.saturating_add(1);
        }

        if session.is_none() {
            match HostSession::start(&settings) {
                Ok(new_session) => {
                    let mut lock = status.lock();
                    lock.loaded = true;
                    lock.host_pid = Some(new_session.pid());
                    lock.health = PluginHostHealth::Healthy;
                    lock.last_error = None;
                    session = Some(new_session);
                }
                Err(error) => {
                    let mut lock = status.lock();
                    lock.error_count = lock.error_count.saturating_add(1);
                    lock.health = PluginHostHealth::Failed;
                    lock.loaded = false;
                    lock.host_pid = None;
                    lock.last_error = Some(error.message());
                    let _ = result_tx.try_send(request.samples);
                    continue;
                }
            }
        }

        let mut failed = None;
        if let Some(current) = session.as_mut() {
            match current.process(request.frames, request.channels, request.samples.clone()) {
                Ok(samples) => {
                    let mut lock = status.lock();
                    lock.loaded = true;
                    lock.health = PluginHostHealth::Healthy;
                    lock.host_pid = Some(current.pid());
                    lock.last_error = None;
                    let _ = result_tx.try_send(samples);
                }
                Err(error) => {
                    failed = Some(error);
                }
            }
        }

        if let Some(error) = failed {
            let mut lock = status.lock();
            lock.error_count = lock.error_count.saturating_add(1);
            if error.is_timeout() {
                lock.timeout_count = lock.timeout_count.saturating_add(1);
                lock.health = PluginHostHealth::Degraded;
            } else {
                lock.health = PluginHostHealth::Failed;
            }
            lock.loaded = false;
            lock.host_pid = None;
            lock.last_error = Some(error.message());
            lock.restart_count = lock.restart_count.saturating_add(1);
            drop(lock);

            if let Some(mut current) = session.take() {
                current.terminate();
            }
            let _ = result_tx.try_send(request.samples);
        }
    }

    if let Some(mut current) = session {
        current.shutdown();
    }
}

impl HostSession {
    fn start(settings: &AuWorkerSettings) -> Result<Self, HostSessionError> {
        let socket_path = make_socket_path(&settings.processor_id);
        let mut command = plugin_host_command(&settings.config);
        command
            .arg("--socket")
            .arg(&socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = command.spawn().map_err(|error| {
            HostSessionError::Io(format!("failed to spawn plugin host: {error}"))
        })?;

        let started = Instant::now();
        let startup_timeout = Duration::from_millis(HOST_STARTUP_TIMEOUT_MS);
        let stream = loop {
            match UnixStream::connect(&socket_path) {
                Ok(stream) => break stream,
                Err(error) => {
                    if started.elapsed() >= startup_timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = std::fs::remove_file(&socket_path);
                        return Err(HostSessionError::Timeout(format!(
                            "plugin host did not accept connection in {}ms: {error}",
                            HOST_STARTUP_TIMEOUT_MS
                        )));
                    }
                    thread::sleep(Duration::from_millis(HOST_CONNECT_RETRY_MS));
                }
            }
        };

        stream
            .set_read_timeout(Some(Duration::from_millis(
                settings.config.process_timeout_ms as u64,
            )))
            .map_err(|error| {
                HostSessionError::Io(format!("failed to set plugin host read timeout: {error}"))
            })?;
        stream
            .set_write_timeout(Some(Duration::from_millis(
                settings.config.process_timeout_ms as u64,
            )))
            .map_err(|error| {
                HostSessionError::Io(format!("failed to set plugin host write timeout: {error}"))
            })?;

        let reader_stream = stream.try_clone().map_err(|error| {
            HostSessionError::Io(format!("failed to clone plugin host stream: {error}"))
        })?;

        let mut session = Self {
            child,
            socket_path,
            writer: stream,
            reader: BufReader::new(reader_stream),
            instance_id: settings.processor_id.clone(),
            config: settings.config.clone(),
        };

        session.handshake()?;
        session.load()?;
        session.prepare(settings.sample_rate, settings.channels, settings.max_frames)?;
        Ok(session)
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn handshake(&mut self) -> Result<(), HostSessionError> {
        let response = self.exchange(
            PluginHostRequest::Handshake {
                protocol_version: PLUGIN_HOST_PROTOCOL_VERSION,
            },
            Duration::from_millis(250),
        )?;
        match response {
            PluginHostResponse::Handshake { protocol_version }
                if protocol_version == PLUGIN_HOST_PROTOCOL_VERSION =>
            {
                Ok(())
            }
            PluginHostResponse::Handshake { protocol_version } => {
                Err(HostSessionError::Io(format!(
                    "plugin host protocol mismatch: expected {PLUGIN_HOST_PROTOCOL_VERSION}, got {protocol_version}"
                )))
            }
            PluginHostResponse::Error { message } => Err(HostSessionError::Io(message)),
            other => Err(HostSessionError::Io(format!(
                "unexpected handshake response: {other:?}"
            ))),
        }
    }

    fn load(&mut self) -> Result<(), HostSessionError> {
        let response = self.exchange(
            PluginHostRequest::Load {
                instance_id: self.instance_id.clone(),
                config: self.config.clone(),
            },
            Duration::from_millis(500),
        )?;
        expect_ack(response, "load")
    }

    fn prepare(
        &mut self,
        sample_rate: u32,
        channels: usize,
        max_frames: usize,
    ) -> Result<(), HostSessionError> {
        let response = self.exchange(
            PluginHostRequest::Prepare {
                instance_id: self.instance_id.clone(),
                sample_rate,
                channels: channels as u16,
                max_frames: max_frames as u32,
            },
            Duration::from_millis(500),
        )?;
        expect_ack(response, "prepare")
    }

    fn process(
        &mut self,
        frames: usize,
        channels: usize,
        samples: Vec<f32>,
    ) -> Result<Vec<f32>, HostSessionError> {
        let timeout = Duration::from_millis(self.config.process_timeout_ms as u64);
        let response = self.exchange(
            PluginHostRequest::Process {
                instance_id: self.instance_id.clone(),
                channels: channels as u16,
                frames: frames as u32,
                samples,
            },
            timeout,
        )?;
        match response {
            PluginHostResponse::Processed { samples } => Ok(samples),
            PluginHostResponse::Error { message } => Err(HostSessionError::Io(message)),
            other => Err(HostSessionError::Io(format!(
                "unexpected process response: {other:?}"
            ))),
        }
    }

    fn reset(&mut self) {
        let _ = self.exchange(
            PluginHostRequest::Reset {
                instance_id: self.instance_id.clone(),
            },
            Duration::from_millis(250),
        );
    }

    fn unload(&mut self) {
        let _ = self.exchange(
            PluginHostRequest::Unload {
                instance_id: self.instance_id.clone(),
            },
            Duration::from_millis(250),
        );
    }

    fn shutdown(&mut self) {
        self.reset();
        self.unload();
        let _ = self.exchange(PluginHostRequest::Shutdown, Duration::from_millis(250));
        self.terminate();
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
    }

    fn exchange(
        &mut self,
        request: PluginHostRequest,
        timeout: Duration,
    ) -> Result<PluginHostResponse, HostSessionError> {
        self.writer
            .set_write_timeout(Some(timeout))
            .map_err(|error| {
                HostSessionError::Io(format!("failed to set write timeout: {error}"))
            })?;
        self.reader
            .get_mut()
            .set_read_timeout(Some(timeout))
            .map_err(|error| {
                HostSessionError::Io(format!("failed to set read timeout: {error}"))
            })?;

        let mut payload = serde_json::to_string(&request).map_err(|error| {
            HostSessionError::Io(format!("failed to serialize request: {error}"))
        })?;
        payload.push('\n');
        self.writer
            .write_all(payload.as_bytes())
            .map_err(map_io_timeout)?;
        self.writer.flush().map_err(map_io_timeout)?;

        let mut line = String::new();
        let bytes = self.reader.read_line(&mut line).map_err(map_io_timeout)?;
        if bytes == 0 {
            return Err(HostSessionError::Io(
                "plugin host closed connection unexpectedly".to_string(),
            ));
        }
        serde_json::from_str::<PluginHostResponse>(&line)
            .map_err(|error| HostSessionError::Io(format!("failed to decode response: {error}")))
    }
}

fn expect_ack(response: PluginHostResponse, operation: &str) -> Result<(), HostSessionError> {
    match response {
        PluginHostResponse::Ack => Ok(()),
        PluginHostResponse::Error { message } => Err(HostSessionError::Io(format!(
            "plugin host {operation} failed: {message}"
        ))),
        other => Err(HostSessionError::Io(format!(
            "unexpected {operation} response: {other:?}"
        ))),
    }
}

fn map_io_timeout(error: std::io::Error) -> HostSessionError {
    match error.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            HostSessionError::Timeout(format!("plugin host request timed out: {error}"))
        }
        _ => HostSessionError::Io(format!("plugin host io error: {error}")),
    }
}

fn plugin_host_command(config: &AuPluginConfig) -> Command {
    if !config.host_command.trim().is_empty() {
        let mut command = Command::new(config.host_command.trim());
        for arg in &config.host_args {
            command.arg(arg);
        }
        return command;
    }

    let binary =
        env::var("MARS_PLUGIN_HOST_BIN").unwrap_or_else(|_| "mars-plugin-host".to_string());
    let mut command = Command::new(binary);
    if let Ok(args) = env::var("MARS_PLUGIN_HOST_ARGS") {
        for arg in args.split_whitespace() {
            if !arg.is_empty() {
                command.arg(arg);
            }
        }
    }
    command
}

fn make_socket_path(processor_id: &str) -> PathBuf {
    const UNIX_SOCKET_PATH_LIMIT: usize = 103;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mut safe_id = processor_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe_id.len() > 16 {
        safe_id.truncate(16);
    }

    let mut path = PathBuf::from("/tmp");
    path.push(format!("marsph-{pid}-{safe_id}-{ts:x}.sock"));
    if path.to_string_lossy().len() <= UNIX_SOCKET_PATH_LIMIT {
        return path;
    }

    let mut hasher = DefaultHasher::new();
    processor_id.hash(&mut hasher);
    let digest = hasher.finish();
    let mut fallback = PathBuf::from("/tmp");
    fallback.push(format!("mph-{pid}-{digest:x}-{ts:x}.sock"));
    fallback
}

#[cfg(test)]
mod tests {
    use super::make_socket_path;

    #[test]
    fn socket_path_respects_unix_length_limit() {
        let path = make_socket_path(
            "processor-with-long-id-and-extra-segments-that-would-overflow-standard-unix-path",
        );
        assert!(path.to_string_lossy().len() <= 103);
    }
}

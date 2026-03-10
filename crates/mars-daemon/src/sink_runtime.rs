use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use mars_types::{
    FileSinkFormat, SinkRuntimeHealth, SinkRuntimeKind, SinkRuntimeSinkStatus, SinkRuntimeStatus,
    StreamTransport,
};
use parking_lot::Mutex;

const WORKER_POLL_INTERVAL_MS: u64 = 25;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkBindingKind {
    File {
        path: String,
        format: FileSinkFormat,
    },
    Stream {
        transport: StreamTransport,
        endpoint: String,
        options_json: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkBinding {
    pub id: String,
    pub source: String,
    pub channels: u16,
    pub kind: SinkBindingKind,
}

#[derive(Debug)]
struct SinkBatch {
    sink_id: String,
    frames: usize,
    samples: Vec<f32>,
}

#[derive(Debug, Clone)]
struct SinkRecord {
    id: String,
    source: String,
    kind: SinkRuntimeKind,
    health: SinkRuntimeHealth,
    written_frames: u64,
    dropped_batches: u64,
    last_error: Option<String>,
}

#[derive(Debug)]
struct SinkRuntimeState {
    queue_capacity: usize,
    queued_batches: usize,
    dropped_batches: u64,
    dropped_samples: u64,
    write_errors: u64,
    sinks: BTreeMap<String, SinkRecord>,
}

impl SinkRuntimeState {
    fn new(queue_capacity: usize, bindings: &[SinkBinding]) -> Self {
        let mut sinks = BTreeMap::new();
        for binding in bindings {
            let kind = match binding.kind {
                SinkBindingKind::File { .. } => SinkRuntimeKind::File,
                SinkBindingKind::Stream { .. } => SinkRuntimeKind::Stream,
            };
            sinks.insert(
                binding.id.clone(),
                SinkRecord {
                    id: binding.id.clone(),
                    source: binding.source.clone(),
                    kind,
                    health: SinkRuntimeHealth::Healthy,
                    written_frames: 0,
                    dropped_batches: 0,
                    last_error: None,
                },
            );
        }
        Self {
            queue_capacity,
            queued_batches: 0,
            dropped_batches: 0,
            dropped_samples: 0,
            write_errors: 0,
            sinks,
        }
    }

    fn mark_sink_failed(&mut self, sink_id: &str, error: impl Into<String>) {
        self.write_errors = self.write_errors.saturating_add(1);
        if let Some(record) = self.sinks.get_mut(sink_id) {
            record.health = SinkRuntimeHealth::Failed;
            record.last_error = Some(error.into());
        }
    }

    fn mark_sink_degraded(&mut self, sink_id: &str, error: impl Into<String>) {
        if let Some(record) = self.sinks.get_mut(sink_id) {
            record.health = SinkRuntimeHealth::Degraded;
            record.last_error = Some(error.into());
        }
    }

    fn mark_drop(&mut self, sink_id: &str, dropped_samples: usize) {
        self.dropped_batches = self.dropped_batches.saturating_add(1);
        self.dropped_samples = self.dropped_samples.saturating_add(dropped_samples as u64);
        if let Some(record) = self.sinks.get_mut(sink_id) {
            record.dropped_batches = record.dropped_batches.saturating_add(1);
            if record.health == SinkRuntimeHealth::Healthy {
                record.health = SinkRuntimeHealth::Degraded;
            }
        }
    }

    fn mark_write_success(&mut self, sink_id: &str, frames: usize) {
        if let Some(record) = self.sinks.get_mut(sink_id) {
            record.written_frames = record.written_frames.saturating_add(frames as u64);
            if record.health == SinkRuntimeHealth::Degraded {
                record.health = SinkRuntimeHealth::Healthy;
            }
        }
    }

    fn mark_write_error(&mut self, sink_id: &str, error: impl Into<String>) {
        self.write_errors = self.write_errors.saturating_add(1);
        if let Some(record) = self.sinks.get_mut(sink_id) {
            record.health = SinkRuntimeHealth::Failed;
            record.last_error = Some(error.into());
        }
    }

    fn status(&self) -> SinkRuntimeStatus {
        let sinks = self
            .sinks
            .values()
            .cloned()
            .map(|record| SinkRuntimeSinkStatus {
                id: record.id,
                source: record.source,
                kind: record.kind,
                health: record.health,
                written_frames: record.written_frames,
                dropped_batches: record.dropped_batches,
                last_error: record.last_error,
            })
            .collect::<Vec<_>>();
        let active_file_sinks = sinks
            .iter()
            .filter(|sink| {
                sink.kind == SinkRuntimeKind::File && sink.health == SinkRuntimeHealth::Healthy
            })
            .count();
        let active_stream_sinks = sinks
            .iter()
            .filter(|sink| {
                sink.kind == SinkRuntimeKind::Stream && sink.health == SinkRuntimeHealth::Healthy
            })
            .count();
        SinkRuntimeStatus {
            queue_capacity: self.queue_capacity,
            queued_batches: self.queued_batches,
            dropped_batches: self.dropped_batches,
            dropped_samples: self.dropped_samples,
            write_errors: self.write_errors,
            active_file_sinks,
            active_stream_sinks,
            sinks,
        }
    }
}

#[derive(Debug)]
pub struct SinkRuntimeSubmitter {
    bindings: Vec<SinkBinding>,
    sender: SyncSender<SinkBatch>,
    state: Arc<Mutex<SinkRuntimeState>>,
}

impl SinkRuntimeSubmitter {
    pub fn submit_rendered_sinks(&self, rendered_sinks: &HashMap<String, Vec<f32>>, frames: usize) {
        for binding in &self.bindings {
            let Some(data) = rendered_sinks.get(&binding.source) else {
                self.state.lock().mark_sink_degraded(
                    &binding.id,
                    format!("missing rendered source '{}'", binding.source),
                );
                continue;
            };

            let batch = SinkBatch {
                sink_id: binding.id.clone(),
                frames,
                samples: data.clone(),
            };
            match self.sender.try_send(batch) {
                Ok(()) => {
                    let mut state = self.state.lock();
                    state.queued_batches = state.queued_batches.saturating_add(1);
                }
                Err(TrySendError::Full(batch)) => {
                    self.state
                        .lock()
                        .mark_drop(&binding.id, batch.samples.len());
                }
                Err(TrySendError::Disconnected(batch)) => {
                    let mut state = self.state.lock();
                    state.mark_sink_failed(&binding.id, "sink runtime worker disconnected");
                    state.mark_drop(&binding.id, batch.samples.len());
                }
            }
        }
    }

    pub fn status(&self) -> SinkRuntimeStatus {
        self.state.lock().status()
    }
}

#[derive(Debug)]
pub struct SinkRuntime {
    submitter: Arc<SinkRuntimeSubmitter>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SinkRuntime {
    pub fn start(
        bindings: Vec<SinkBinding>,
        sample_rate: u32,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        let queue_capacity = queue_capacity.max(1);
        let state = Arc::new(Mutex::new(SinkRuntimeState::new(queue_capacity, &bindings)));
        let (sender, receiver) = sync_channel::<SinkBatch>(queue_capacity);
        let submitter = Arc::new(SinkRuntimeSubmitter {
            bindings: bindings.clone(),
            sender,
            state: state.clone(),
        });
        let mut writers = HashMap::<String, Box<dyn SinkWriter>>::new();

        for binding in &bindings {
            match &binding.kind {
                SinkBindingKind::File { path, format } => {
                    match FileSinkWriter::open(path, *format, sample_rate, binding.channels) {
                        Ok(writer) => {
                            writers.insert(binding.id.clone(), Box::new(writer));
                        }
                        Err(error) => {
                            state.lock().mark_sink_failed(&binding.id, error);
                        }
                    }
                }
                SinkBindingKind::Stream {
                    transport,
                    endpoint,
                    options_json,
                } => {
                    let message = format!(
                        "stream sink not implemented for transport={transport:?}, endpoint={endpoint}, options={options_json}"
                    );
                    state.lock().mark_sink_failed(&binding.id, message);
                }
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_state = state.clone();
        let handle = std::thread::Builder::new()
            .name("marsd-sink".to_string())
            .spawn(move || {
                sink_worker_loop(receiver, writers, thread_state, thread_stop);
            })
            .map_err(|error| format!("failed to spawn sink runtime thread: {error}"))?;

        Ok(Self {
            submitter,
            stop,
            handle: Some(handle),
        })
    }

    pub fn submitter(&self) -> Arc<SinkRuntimeSubmitter> {
        self.submitter.clone()
    }

    pub fn status(&self) -> SinkRuntimeStatus {
        self.submitter.status()
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn sink_worker_loop(
    receiver: Receiver<SinkBatch>,
    mut writers: HashMap<String, Box<dyn SinkWriter>>,
    state: Arc<Mutex<SinkRuntimeState>>,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::Relaxed) && state.lock().queued_batches == 0 {
            break;
        }

        match receiver.recv_timeout(Duration::from_millis(WORKER_POLL_INTERVAL_MS)) {
            Ok(batch) => {
                {
                    let mut lock = state.lock();
                    lock.queued_batches = lock.queued_batches.saturating_sub(1);
                }

                let Some(writer) = writers.get_mut(&batch.sink_id) else {
                    state
                        .lock()
                        .mark_sink_failed(&batch.sink_id, "sink writer unavailable");
                    continue;
                };

                match writer.write_interleaved(&batch.samples, batch.frames) {
                    Ok(()) => state
                        .lock()
                        .mark_write_success(&batch.sink_id, batch.frames),
                    Err(error) => state.lock().mark_write_error(&batch.sink_id, error),
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    for (sink_id, writer) in &mut writers {
        if let Err(error) = writer.finalize() {
            state.lock().mark_write_error(sink_id, error);
        }
    }
}

trait SinkWriter: Send {
    fn write_interleaved(&mut self, samples: &[f32], frames: usize) -> Result<(), String>;
    fn finalize(&mut self) -> Result<(), String>;
}

struct FileSinkWriter {
    inner: FileSinkInner,
    channels: u16,
}

enum FileSinkInner {
    Wav(WavWriter),
    Caf(CafWriter),
}

impl FileSinkWriter {
    fn open(
        path: &str,
        format: FileSinkFormat,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Self, String> {
        if channels == 0 {
            return Err("sink channels must be > 0".to_string());
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "failed to create sink directory '{}': {error}",
                        parent.display()
                    )
                })?;
            }
        }

        let inner = match format {
            FileSinkFormat::Wav => {
                FileSinkInner::Wav(WavWriter::open(path, sample_rate, channels)?)
            }
            FileSinkFormat::Caf => {
                FileSinkInner::Caf(CafWriter::open(path, sample_rate, channels)?)
            }
        };
        Ok(Self { inner, channels })
    }
}

impl SinkWriter for FileSinkWriter {
    fn write_interleaved(&mut self, samples: &[f32], frames: usize) -> Result<(), String> {
        let expected = frames.saturating_mul(self.channels as usize);
        if samples.len() != expected {
            return Err(format!(
                "sink sample count mismatch: expected {expected}, got {}",
                samples.len()
            ));
        }
        match &mut self.inner {
            FileSinkInner::Wav(writer) => writer.write_interleaved(samples),
            FileSinkInner::Caf(writer) => writer.write_interleaved(samples),
        }
    }

    fn finalize(&mut self) -> Result<(), String> {
        match &mut self.inner {
            FileSinkInner::Wav(writer) => writer.finalize(),
            FileSinkInner::Caf(writer) => writer.finalize(),
        }
    }
}

struct WavWriter {
    file: BufWriter<File>,
    data_bytes: u64,
    scratch: Vec<u8>,
}

impl WavWriter {
    fn open(path: &str, sample_rate: u32, channels: u16) -> Result<Self, String> {
        let mut file = BufWriter::new(
            File::create(path)
                .map_err(|error| format!("failed to create wav sink '{path}': {error}"))?,
        );
        let block_align = channels.saturating_mul(4);
        let byte_rate = sample_rate.saturating_mul(block_align as u32);

        file.write_all(b"RIFF")
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&0u32.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(b"WAVE")
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(b"fmt ")
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&16u32.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&3u16.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&channels.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&sample_rate.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&byte_rate.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&block_align.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&32u16.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(b"data")
            .map_err(|error| format!("failed to write wav header: {error}"))?;
        file.write_all(&0u32.to_le_bytes())
            .map_err(|error| format!("failed to write wav header: {error}"))?;

        Ok(Self {
            file,
            data_bytes: 0,
            scratch: Vec::new(),
        })
    }

    fn write_interleaved(&mut self, samples: &[f32]) -> Result<(), String> {
        self.scratch.clear();
        self.scratch.reserve(samples.len().saturating_mul(4));
        for sample in samples {
            self.scratch.extend_from_slice(&sample.to_le_bytes());
        }
        self.file
            .write_all(&self.scratch)
            .map_err(|error| format!("failed to write wav samples: {error}"))?;
        self.data_bytes = self
            .data_bytes
            .saturating_add((samples.len().saturating_mul(4)) as u64);
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), String> {
        if self.data_bytes > u32::MAX as u64 {
            return Err("wav sink exceeded 4GB data chunk size".to_string());
        }
        self.file
            .flush()
            .map_err(|error| format!("failed to flush wav sink: {error}"))?;
        let file = self.file.get_mut();
        let riff_size = 36u32.saturating_add(self.data_bytes as u32);
        file.seek(SeekFrom::Start(4))
            .map_err(|error| format!("failed to seek wav header: {error}"))?;
        file.write_all(&riff_size.to_le_bytes())
            .map_err(|error| format!("failed to patch wav riff size: {error}"))?;
        file.seek(SeekFrom::Start(40))
            .map_err(|error| format!("failed to seek wav data size: {error}"))?;
        file.write_all(&(self.data_bytes as u32).to_le_bytes())
            .map_err(|error| format!("failed to patch wav data size: {error}"))?;
        file.flush()
            .map_err(|error| format!("failed to flush wav sink file: {error}"))?;
        Ok(())
    }
}

struct CafWriter {
    file: BufWriter<File>,
    data_bytes: u64,
    data_chunk_size_pos: u64,
    scratch: Vec<u8>,
}

impl CafWriter {
    fn open(path: &str, sample_rate: u32, channels: u16) -> Result<Self, String> {
        let mut file = BufWriter::new(
            File::create(path)
                .map_err(|error| format!("failed to create caf sink '{path}': {error}"))?,
        );

        file.write_all(b"caff")
            .map_err(|error| format!("failed to write caf header: {error}"))?;
        file.write_all(&1u16.to_be_bytes())
            .map_err(|error| format!("failed to write caf header: {error}"))?;
        file.write_all(&0u16.to_be_bytes())
            .map_err(|error| format!("failed to write caf header: {error}"))?;

        file.write_all(b"desc")
            .map_err(|error| format!("failed to write caf desc chunk: {error}"))?;
        file.write_all(&32u64.to_be_bytes())
            .map_err(|error| format!("failed to write caf desc chunk: {error}"))?;
        file.write_all(&(sample_rate as f64).to_be_bytes())
            .map_err(|error| format!("failed to write caf desc sample rate: {error}"))?;
        file.write_all(b"lpcm")
            .map_err(|error| format!("failed to write caf desc format: {error}"))?;
        let format_flags = (1u32) | (1u32 << 1) | (1u32 << 3);
        file.write_all(&format_flags.to_be_bytes())
            .map_err(|error| format!("failed to write caf desc format flags: {error}"))?;
        let bytes_per_frame = (channels as u32).saturating_mul(4);
        file.write_all(&bytes_per_frame.to_be_bytes())
            .map_err(|error| format!("failed to write caf desc bytes per packet: {error}"))?;
        file.write_all(&1u32.to_be_bytes())
            .map_err(|error| format!("failed to write caf desc frames per packet: {error}"))?;
        file.write_all(&(channels as u32).to_be_bytes())
            .map_err(|error| format!("failed to write caf desc channels per frame: {error}"))?;
        file.write_all(&32u32.to_be_bytes())
            .map_err(|error| format!("failed to write caf desc bits per channel: {error}"))?;

        file.write_all(b"data")
            .map_err(|error| format!("failed to write caf data chunk: {error}"))?;
        let data_chunk_size_pos = file
            .stream_position()
            .map_err(|error| format!("failed to read caf data chunk position: {error}"))?;
        file.write_all(&0u64.to_be_bytes())
            .map_err(|error| format!("failed to write caf data chunk size: {error}"))?;
        file.write_all(&0u32.to_be_bytes())
            .map_err(|error| format!("failed to write caf edit count: {error}"))?;

        Ok(Self {
            file,
            data_bytes: 0,
            data_chunk_size_pos,
            scratch: Vec::new(),
        })
    }

    fn write_interleaved(&mut self, samples: &[f32]) -> Result<(), String> {
        self.scratch.clear();
        self.scratch.reserve(samples.len().saturating_mul(4));
        for sample in samples {
            self.scratch.extend_from_slice(&sample.to_le_bytes());
        }
        self.file
            .write_all(&self.scratch)
            .map_err(|error| format!("failed to write caf samples: {error}"))?;
        self.data_bytes = self
            .data_bytes
            .saturating_add((samples.len().saturating_mul(4)) as u64);
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), String> {
        self.file
            .flush()
            .map_err(|error| format!("failed to flush caf sink: {error}"))?;
        let file = self.file.get_mut();
        let chunk_size = self.data_bytes.saturating_add(4);
        file.seek(SeekFrom::Start(self.data_chunk_size_pos))
            .map_err(|error| format!("failed to seek caf data chunk size: {error}"))?;
        file.write_all(&chunk_size.to_be_bytes())
            .map_err(|error| format!("failed to patch caf data chunk size: {error}"))?;
        file.flush()
            .map_err(|error| format!("failed to flush caf sink file: {error}"))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use mars_types::FileSinkFormat;

    use super::{CafWriter, SinkBinding, SinkBindingKind, SinkRuntime, WavWriter};

    fn temp_file_path(ext: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        std::env::temp_dir().join(format!("mars-sink-{ts}-{}.{}", std::process::id(), ext))
    }

    #[test]
    fn wav_writer_patches_header_sizes_on_finalize() {
        let path = temp_file_path("wav");
        let mut writer =
            WavWriter::open(path.to_str().expect("utf8 path"), 48_000, 2).expect("wav writer");
        writer
            .write_interleaved(&[0.1, -0.1, 0.2, -0.2])
            .expect("write samples");
        writer.finalize().expect("finalize");

        let bytes = fs::read(&path).expect("read wav");
        assert!(bytes.starts_with(b"RIFF"));
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");

        let riff_size = u32::from_le_bytes(bytes[4..8].try_into().expect("riff size"));
        let data_size = u32::from_le_bytes(bytes[40..44].try_into().expect("data size"));
        assert_eq!(data_size, 16);
        assert_eq!(riff_size, 36 + data_size);
        assert_eq!(bytes.len() as u32, 44 + data_size);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn caf_writer_patches_data_chunk_size_on_finalize() {
        let path = temp_file_path("caf");
        let mut writer =
            CafWriter::open(path.to_str().expect("utf8 path"), 48_000, 1).expect("caf writer");
        writer
            .write_interleaved(&[0.1, -0.1, 0.2])
            .expect("write samples");
        writer.finalize().expect("finalize");

        let bytes = fs::read(&path).expect("read caf");
        assert!(bytes.starts_with(b"caff"));
        assert_eq!(&bytes[8..12], b"desc");
        assert_eq!(&bytes[52..56], b"data");
        let data_chunk_size = u64::from_be_bytes(bytes[56..64].try_into().expect("chunk size"));
        assert_eq!(data_chunk_size, 16);
        assert_eq!(bytes.len(), 68 + 12);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sink_runtime_reports_backpressure_with_small_queue() {
        let path = temp_file_path("wav");
        let runtime = SinkRuntime::start(
            vec![SinkBinding {
                id: "sink-main".to_string(),
                source: "mix".to_string(),
                channels: 2,
                kind: SinkBindingKind::File {
                    path: path.to_string_lossy().to_string(),
                    format: FileSinkFormat::Wav,
                },
            }],
            48_000,
            2,
        )
        .expect("sink runtime");
        let submitter = runtime.submitter();
        let mut rendered = HashMap::new();
        rendered.insert("mix".to_string(), vec![0.0_f32; 256 * 2]);

        for _ in 0..20_000 {
            submitter.submit_rendered_sinks(&rendered, 256);
        }
        thread::sleep(Duration::from_millis(80));

        let status = runtime.status();
        assert!(status.dropped_batches > 0);
        runtime.stop();
        let _ = fs::remove_file(path);
    }

    #[test]
    fn sink_runtime_stop_and_restart_preserve_wav_header_validity() {
        let path = temp_file_path("wav");
        for _ in 0..2 {
            let runtime = SinkRuntime::start(
                vec![SinkBinding {
                    id: "sink-main".to_string(),
                    source: "mix".to_string(),
                    channels: 2,
                    kind: SinkBindingKind::File {
                        path: path.to_string_lossy().to_string(),
                        format: FileSinkFormat::Wav,
                    },
                }],
                48_000,
                128,
            )
            .expect("sink runtime");
            let submitter = runtime.submitter();
            let mut rendered = HashMap::new();
            rendered.insert("mix".to_string(), vec![0.25_f32; 128 * 2]);
            for _ in 0..256 {
                submitter.submit_rendered_sinks(&rendered, 128);
            }

            for _ in 0..200 {
                if runtime.status().queued_batches == 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            runtime.stop();

            let bytes = fs::read(&path).expect("read wav");
            assert!(bytes.starts_with(b"RIFF"));
            let data_size = u32::from_le_bytes(bytes[40..44].try_into().expect("data size"));
            assert!(data_size > 0);
            assert_eq!(bytes.len() as u32, 44 + data_size);
        }

        let _ = fs::remove_file(path);
    }
}

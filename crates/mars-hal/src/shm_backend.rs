//! POSIX shared-memory backed ring buffers for MARS streams.
//!
//! macOS POSIX SHM objects are mmap-oriented, so this implementation maps the
//! object and reads/writes bytes directly from the shared region.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::sync::Arc;

use dashmap::DashMap;
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap, shm_open, shm_unlink};
use nix::sys::stat::Mode;
use nix::unistd::ftruncate;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Header magic (`MARS`).
pub const RING_MAGIC: u32 = 0x4D_41_52_53;
/// Header schema version.
pub const RING_VERSION: u32 = 1;

const HEADER_SIZE: usize = 52;
const OFFSET_MAGIC: usize = 0;
const OFFSET_VERSION: usize = 4;
const OFFSET_SAMPLE_RATE: usize = 8;
const OFFSET_CHANNELS: usize = 12;
const OFFSET_CAPACITY: usize = 16;
const OFFSET_WRITE_IDX: usize = 20;
const OFFSET_READ_IDX: usize = 28;
const OFFSET_OVERRUN: usize = 36;
const OFFSET_UNDERRUN: usize = 44;

/// Stream direction controls naming conventions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StreamDirection {
    /// App -> MARS direction.
    Vout,
    /// MARS -> App direction.
    Vin,
}

/// Ring configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RingSpec {
    /// Audio sample rate.
    pub sample_rate: u32,
    /// Channel count in interleaved layout.
    pub channels: u16,
    /// Capacity in frames.
    pub capacity_frames: u32,
}

impl RingSpec {
    #[must_use]
    pub const fn capacity_samples(self) -> usize {
        self.capacity_frames as usize * self.channels as usize
    }

    #[must_use]
    pub const fn data_size_bytes(self) -> usize {
        self.capacity_samples() * std::mem::size_of::<f32>()
    }

    #[must_use]
    pub const fn total_size_bytes(self) -> usize {
        HEADER_SIZE + self.data_size_bytes()
    }
}

/// Shared ring header (serialized in little endian inside SHM object).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RingHeader {
    /// Header magic.
    pub magic: u32,
    /// Header version.
    pub version: u32,
    /// Stream sample rate.
    pub sample_rate: u32,
    /// Stream channels.
    pub channels: u16,
    /// Capacity in frames.
    pub capacity_frames: u32,
    /// Total frames written so far.
    pub write_idx: u64,
    /// Total frames read so far.
    pub read_idx: u64,
    /// Overrun counter.
    pub overrun_count: u64,
    /// Underrun counter.
    pub underrun_count: u64,
}

impl RingHeader {
    #[must_use]
    pub fn new(spec: RingSpec) -> Self {
        Self {
            magic: RING_MAGIC,
            version: RING_VERSION,
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            capacity_frames: spec.capacity_frames,
            write_idx: 0,
            read_idx: 0,
            overrun_count: 0,
            underrun_count: 0,
        }
    }
}

#[derive(Debug)]
struct ShmMap {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: `ShmMap` owns a process-local mmap region pointer and length. Sharing
// between threads is mediated by `Mutex<SharedRing>` in this crate, so moving the
// handle across threads is safe under external synchronization.
unsafe impl Send for ShmMap {}

impl ShmMap {
    fn new(fd: &OwnedFd, len: usize) -> Result<Self, RingError> {
        let len_nz = NonZeroUsize::new(len)
            .ok_or_else(|| RingError::Shm("cannot mmap zero-length shared memory".to_string()))?;

        // SAFETY: `fd` is a valid open descriptor from `shm_open`; we map exactly
        // `len` bytes with read/write permissions in shared mode at offset 0.
        let mapped = unsafe {
            mmap(
                None,
                len_nz,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
        }
        .map_err(|error| RingError::Shm(format!("mmap failed: {error}")))?;

        Ok(Self {
            ptr: mapped.cast::<u8>(),
            len,
        })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr/len` come from successful `mmap` and remain valid for the
        // lifetime of this mapping.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    fn as_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ptr/len` come from successful `mmap`; `&mut self` guarantees
        // unique mutable access in this process.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for ShmMap {
    fn drop(&mut self) {
        // SAFETY: mapping was created by `mmap` and `ptr/len` are unchanged.
        let _ = unsafe { munmap(self.ptr.cast::<std::ffi::c_void>(), self.len) };
    }
}

/// POSIX shared-memory ring object.
#[derive(Debug)]
pub struct SharedRing {
    shm_name: String,
    fd: OwnedFd,
    map: ShmMap,
    spec: RingSpec,
}

impl SharedRing {
    fn create_or_open(public_name: &str, spec: RingSpec) -> Result<Self, RingError> {
        let shm_name = to_posix_shm_name(public_name)?;
        let (mut fd, mut created) = open_shm_fd(&shm_name)?;
        let expected_len = spec.total_size_bytes();

        if created {
            ftruncate(&fd, expected_len as i64)
                .map_err(|error| RingError::Shm(format!("ftruncate failed: {error}")))?;
        }

        let mut map = match ShmMap::new(&fd, expected_len) {
            Ok(map) => map,
            Err(error) if !created => {
                let _ = shm_unlink(shm_name.as_str());
                let reopened = open_shm_fd(&shm_name)?;
                fd = reopened.0;
                created = reopened.1;
                if !created {
                    return Err(RingError::Shm(format!(
                        "failed to recreate incompatible shm object {shm_name}"
                    )));
                }
                ftruncate(&fd, expected_len as i64).map_err(|truncate_error| {
                    RingError::Shm(format!("ftruncate recreate failed: {truncate_error}"))
                })?;
                ShmMap::new(&fd, expected_len)?
            }
            Err(error) => return Err(error),
        };

        if created {
            write_header_bytes(map.as_slice_mut(), RingHeader::new(spec))?;
        } else {
            let header = read_header_bytes(map.as_slice())?;
            let valid = header.magic == RING_MAGIC
                && header.version == RING_VERSION
                && header.sample_rate == spec.sample_rate
                && header.channels == spec.channels
                && header.capacity_frames == spec.capacity_frames;
            if !valid {
                write_header_bytes(map.as_slice_mut(), RingHeader::new(spec))?;
            }
        }

        Ok(Self {
            shm_name,
            fd,
            map,
            spec,
        })
    }

    /// Read current ring header.
    pub fn header(&self) -> Result<RingHeader, RingError> {
        read_header_bytes(self.map.as_slice())
    }

    /// Write interleaved frames into ring.
    pub fn write_interleaved(&mut self, interleaved: &[f32]) -> Result<usize, RingError> {
        let channels = self.spec.channels as usize;
        if channels == 0 {
            return Err(RingError::InvalidChannels);
        }
        if !interleaved.len().is_multiple_of(channels) {
            return Err(RingError::SampleCountNotAligned {
                sample_count: interleaved.len(),
                channels,
            });
        }

        let frames = interleaved.len() / channels;
        if frames == 0 {
            return Ok(0);
        }

        let mut header = read_header_bytes(self.map.as_slice())?;
        let mut frame_bytes = vec![0_u8; channels * std::mem::size_of::<f32>()];

        for frame_idx in 0..frames {
            let used = header.write_idx.saturating_sub(header.read_idx) as usize;
            if used >= header.capacity_frames as usize {
                header.read_idx = header.read_idx.saturating_add(1);
                header.overrun_count = header.overrun_count.saturating_add(1);
            }

            let slot = (header.write_idx % header.capacity_frames as u64) as usize;
            let src = &interleaved[frame_idx * channels..(frame_idx + 1) * channels];
            encode_frame(src, &mut frame_bytes);

            let offset = data_offset(slot, channels)?;
            let end = offset + frame_bytes.len();
            let bytes = self.map.as_slice_mut();
            if end > bytes.len() {
                return Err(RingError::OutOfBounds {
                    requested: end,
                    available: bytes.len(),
                });
            }

            bytes[offset..end].copy_from_slice(&frame_bytes);
            header.write_idx = header.write_idx.saturating_add(1);
        }

        write_header_bytes(self.map.as_slice_mut(), header)?;
        Ok(frames)
    }

    /// Read interleaved frames from ring. Missing frames are zero-filled.
    pub fn read_interleaved(&mut self, out: &mut [f32]) -> Result<usize, RingError> {
        let channels = self.spec.channels as usize;
        if channels == 0 {
            return Err(RingError::InvalidChannels);
        }
        if !out.len().is_multiple_of(channels) {
            return Err(RingError::SampleCountNotAligned {
                sample_count: out.len(),
                channels,
            });
        }

        let requested_frames = out.len() / channels;
        if requested_frames == 0 {
            return Ok(0);
        }

        let mut header = read_header_bytes(self.map.as_slice())?;
        let available_frames =
            (header.write_idx.saturating_sub(header.read_idx) as usize).min(requested_frames);
        let mut frame_bytes = vec![0_u8; channels * std::mem::size_of::<f32>()];

        for frame_idx in 0..available_frames {
            let slot = (header.read_idx % header.capacity_frames as u64) as usize;
            let offset = data_offset(slot, channels)?;
            let end = offset + frame_bytes.len();
            let bytes = self.map.as_slice();
            if end > bytes.len() {
                return Err(RingError::OutOfBounds {
                    requested: end,
                    available: bytes.len(),
                });
            }

            frame_bytes.copy_from_slice(&bytes[offset..end]);
            decode_frame(
                &frame_bytes,
                &mut out[frame_idx * channels..(frame_idx + 1) * channels],
            );
            header.read_idx = header.read_idx.saturating_add(1);
        }

        if available_frames < requested_frames {
            out[available_frames * channels..].fill(0.0);
            header.underrun_count = header.underrun_count.saturating_add(1);
        }

        write_header_bytes(self.map.as_slice_mut(), header)?;
        Ok(available_frames)
    }

    fn unlink(&self) -> Result<bool, RingError> {
        match shm_unlink(self.shm_name.as_str()) {
            Ok(()) => Ok(true),
            Err(Errno::ENOENT) => Ok(false),
            Err(error) => Err(RingError::Shm(format!(
                "shm_unlink {}: {error}",
                self.shm_name
            ))),
        }
    }

    #[allow(dead_code)]
    fn _fd(&self) -> &OwnedFd {
        &self.fd
    }
}

/// Errors from SHM ring operations.
#[derive(Debug, Error)]
pub enum RingError {
    /// Invalid channel count.
    #[error("stream channels cannot be zero")]
    InvalidChannels,
    /// Sample alignment mismatch.
    #[error("sample count {sample_count} is not aligned with channels {channels}")]
    SampleCountNotAligned {
        sample_count: usize,
        channels: usize,
    },
    /// SHM system call failure.
    #[error("shared memory operation failed: {0}")]
    Shm(String),
    /// Invalid or unsupported SHM object name.
    #[error("invalid shared memory name: {0}")]
    InvalidName(String),
    /// Existing SHM object has unexpected header.
    #[error("shared memory header is corrupted for stream '{name}'")]
    CorruptedHeader { name: String },
    /// Existing SHM object conflicts with requested spec.
    #[error("ring spec mismatch for '{name}': expected {expected:?}, actual {actual:?}")]
    SpecMismatch {
        name: String,
        expected: RingSpec,
        actual: RingSpec,
    },
    /// Attempted to read/write beyond mapped size.
    #[error("shared memory out of bounds: requested {requested}, available {available}")]
    OutOfBounds { requested: usize, available: usize },
}

/// Thread-safe handle to a shared ring.
pub type SharedRingHandle = Arc<Mutex<SharedRing>>;

/// Per-process ring handle registry.
#[derive(Debug, Default)]
pub struct RingRegistry {
    rings: DashMap<String, SharedRingHandle>,
}

impl RingRegistry {
    /// Create or open a named ring.
    pub fn create_or_open(
        &self,
        name: &str,
        spec: RingSpec,
    ) -> Result<SharedRingHandle, RingError> {
        if let Some(existing) = self.rings.get(name) {
            return Ok(existing.clone());
        }

        let ring = SharedRing::create_or_open(name, spec)?;
        let handle = Arc::new(Mutex::new(ring));
        self.rings.insert(name.to_string(), handle.clone());

        register_name(name);
        Ok(handle)
    }

    /// Open an existing ring if it is already in this process registry.
    #[must_use]
    pub fn open(&self, name: &str) -> Option<SharedRingHandle> {
        self.rings.get(name).map(|entry| entry.clone())
    }

    /// Unlink and remove a ring by public name.
    pub fn remove(&self, name: &str) -> bool {
        let mut removed = false;

        if let Some((_, handle)) = self.rings.remove(name) {
            let guard = handle.lock();
            removed = guard.unlink().unwrap_or(false);
        }

        if let Ok(shm_name) = to_posix_shm_name(name) {
            match shm_unlink(shm_name.as_str()) {
                Ok(()) => removed = true,
                Err(Errno::ENOENT) => {}
                Err(_) => {}
            }
        }

        unregister_name(name);
        removed
    }

    /// Remove all rings that match a prefix.
    pub fn remove_namespace(&self, prefix: &str) -> usize {
        let mut targets = self
            .rings
            .iter()
            .filter_map(|entry| {
                if entry.key().starts_with(prefix) {
                    Some(entry.key().clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for manifest_name in registered_names() {
            if manifest_name.starts_with(prefix) {
                targets.push(manifest_name);
            }
        }

        targets.sort();
        targets.dedup();

        let mut removed = 0;
        for name in targets {
            if self.remove(&name) {
                removed += 1;
            }
        }

        removed
    }
}

static GLOBAL_REGISTRY: Lazy<RingRegistry> = Lazy::new(RingRegistry::default);
static NAME_REGISTRY: Lazy<Mutex<BTreeSet<String>>> = Lazy::new(|| Mutex::new(BTreeSet::new()));

/// Global process-local ring registry.
#[must_use]
pub fn global_registry() -> &'static RingRegistry {
    &GLOBAL_REGISTRY
}

/// Build a stream name from direction + uid.
#[must_use]
pub fn stream_name(direction: StreamDirection, uid: &str) -> String {
    match direction {
        StreamDirection::Vout => format!("mars.vout.{uid}"),
        StreamDirection::Vin => format!("mars.vin.{uid}"),
    }
}

fn register_name(name: &str) {
    NAME_REGISTRY.lock().insert(name.to_string());
}

fn unregister_name(name: &str) {
    NAME_REGISTRY.lock().remove(name);
}

fn registered_names() -> Vec<String> {
    NAME_REGISTRY.lock().iter().cloned().collect()
}

fn to_posix_shm_name(public_name: &str) -> Result<String, RingError> {
    if public_name.is_empty() {
        return Err(RingError::InvalidName(public_name.to_string()));
    }

    let mut out = String::with_capacity(public_name.len() + 1);
    out.push('/');
    for ch in public_name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    if out.len() < 2 {
        return Err(RingError::InvalidName(public_name.to_string()));
    }

    Ok(out)
}

fn open_shm_fd(shm_name: &str) -> Result<(OwnedFd, bool), RingError> {
    let mode = Mode::from_bits_truncate(0o600);
    let create_flags = OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR;

    match shm_open(shm_name, create_flags, mode) {
        Ok(fd) => Ok((fd, true)),
        Err(Errno::EEXIST) => shm_open(shm_name, OFlag::O_RDWR, mode)
            .map(|fd| (fd, false))
            .map_err(|error| RingError::Shm(format!("shm_open {shm_name}: {error}"))),
        Err(error) => Err(RingError::Shm(format!("shm_open {shm_name}: {error}"))),
    }
}

fn read_header_bytes(bytes: &[u8]) -> Result<RingHeader, RingError> {
    if bytes.len() < HEADER_SIZE {
        return Err(RingError::OutOfBounds {
            requested: HEADER_SIZE,
            available: bytes.len(),
        });
    }

    Ok(RingHeader {
        magic: u32::from_le_bytes(
            bytes[OFFSET_MAGIC..OFFSET_MAGIC + 4]
                .try_into()
                .expect("slice"),
        ),
        version: u32::from_le_bytes(
            bytes[OFFSET_VERSION..OFFSET_VERSION + 4]
                .try_into()
                .expect("slice"),
        ),
        sample_rate: u32::from_le_bytes(
            bytes[OFFSET_SAMPLE_RATE..OFFSET_SAMPLE_RATE + 4]
                .try_into()
                .expect("slice"),
        ),
        channels: u16::from_le_bytes(
            bytes[OFFSET_CHANNELS..OFFSET_CHANNELS + 2]
                .try_into()
                .expect("slice"),
        ),
        capacity_frames: u32::from_le_bytes(
            bytes[OFFSET_CAPACITY..OFFSET_CAPACITY + 4]
                .try_into()
                .expect("slice"),
        ),
        write_idx: u64::from_le_bytes(
            bytes[OFFSET_WRITE_IDX..OFFSET_WRITE_IDX + 8]
                .try_into()
                .expect("slice"),
        ),
        read_idx: u64::from_le_bytes(
            bytes[OFFSET_READ_IDX..OFFSET_READ_IDX + 8]
                .try_into()
                .expect("slice"),
        ),
        overrun_count: u64::from_le_bytes(
            bytes[OFFSET_OVERRUN..OFFSET_OVERRUN + 8]
                .try_into()
                .expect("slice"),
        ),
        underrun_count: u64::from_le_bytes(
            bytes[OFFSET_UNDERRUN..OFFSET_UNDERRUN + 8]
                .try_into()
                .expect("slice"),
        ),
    })
}

fn write_header_bytes(bytes: &mut [u8], header: RingHeader) -> Result<(), RingError> {
    if bytes.len() < HEADER_SIZE {
        return Err(RingError::OutOfBounds {
            requested: HEADER_SIZE,
            available: bytes.len(),
        });
    }

    bytes[OFFSET_MAGIC..OFFSET_MAGIC + 4].copy_from_slice(&header.magic.to_le_bytes());
    bytes[OFFSET_VERSION..OFFSET_VERSION + 4].copy_from_slice(&header.version.to_le_bytes());
    bytes[OFFSET_SAMPLE_RATE..OFFSET_SAMPLE_RATE + 4]
        .copy_from_slice(&header.sample_rate.to_le_bytes());
    bytes[OFFSET_CHANNELS..OFFSET_CHANNELS + 2].copy_from_slice(&header.channels.to_le_bytes());
    bytes[OFFSET_CAPACITY..OFFSET_CAPACITY + 4]
        .copy_from_slice(&header.capacity_frames.to_le_bytes());
    bytes[OFFSET_WRITE_IDX..OFFSET_WRITE_IDX + 8].copy_from_slice(&header.write_idx.to_le_bytes());
    bytes[OFFSET_READ_IDX..OFFSET_READ_IDX + 8].copy_from_slice(&header.read_idx.to_le_bytes());
    bytes[OFFSET_OVERRUN..OFFSET_OVERRUN + 8].copy_from_slice(&header.overrun_count.to_le_bytes());
    bytes[OFFSET_UNDERRUN..OFFSET_UNDERRUN + 8]
        .copy_from_slice(&header.underrun_count.to_le_bytes());
    Ok(())
}

fn data_offset(slot: usize, channels: usize) -> Result<usize, RingError> {
    let sample_size = std::mem::size_of::<f32>();
    let frame_size = channels
        .checked_mul(sample_size)
        .ok_or_else(|| RingError::Shm("frame size overflow".to_string()))?;
    HEADER_SIZE
        .checked_add(
            slot.checked_mul(frame_size)
                .ok_or_else(|| RingError::Shm("ring offset overflow".to_string()))?,
        )
        .ok_or_else(|| RingError::Shm("ring offset overflow".to_string()))
}

fn encode_frame(samples: &[f32], out: &mut [u8]) {
    for (idx, sample) in samples.iter().enumerate() {
        let bytes = sample.to_le_bytes();
        let start = idx * 4;
        out[start..start + 4].copy_from_slice(&bytes);
    }
}

fn decode_frame(bytes: &[u8], out: &mut [f32]) {
    for (idx, sample) in out.iter_mut().enumerate() {
        let start = idx * 4;
        *sample = f32::from_le_bytes(bytes[start..start + 4].try_into().expect("slice"));
    }
}

#[cfg(test)]
mod tests {
    use super::{RingRegistry, RingSpec, StreamDirection, global_registry, stream_name};

    #[test]
    fn shared_between_independent_registries() {
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 2,
            capacity_frames: 8,
        };
        let name = stream_name(StreamDirection::Vout, "cross-process");

        let registry_a = RingRegistry::default();
        let registry_b = RingRegistry::default();

        let writer = registry_a
            .create_or_open(&name, spec)
            .expect("writer ring available");
        let reader = registry_b
            .create_or_open(&name, spec)
            .expect("reader ring available");

        {
            let mut writer = writer.lock();
            writer
                .write_interleaved(&[0.1, 0.2, 0.3, 0.4])
                .expect("write works");
        }

        {
            let mut out = [0.0_f32; 4];
            let mut reader = reader.lock();
            let got = reader.read_interleaved(&mut out).expect("read works");
            assert_eq!(got, 2);
            assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
        }

        let _ = registry_a.remove(&name);
        let _ = registry_b.remove(&name);
    }

    #[test]
    fn overruns_and_underruns_are_counted() {
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 2,
            capacity_frames: 2,
        };
        let name = stream_name(StreamDirection::Vout, "test");
        let ring = global_registry()
            .create_or_open(&name, spec)
            .expect("create ring");

        {
            let mut guard = ring.lock();
            guard
                .write_interleaved(&[1.0, 1.0, 2.0, 2.0, 3.0, 3.0])
                .expect("write should succeed");
            assert!(guard.header().expect("header").overrun_count >= 1);

            let mut out = [0.0_f32; 6];
            guard
                .read_interleaved(&mut out)
                .expect("read should succeed");
            assert!(guard.header().expect("header").underrun_count >= 1);
        }

        let _ = global_registry().remove(&name);
    }
}

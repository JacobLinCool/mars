//! POSIX shared-memory backed ring buffers for MARS streams.
//!
//! macOS POSIX SHM objects are mmap-oriented, so this implementation maps the
//! object and accesses the shared region directly.
//!
//! # Ring protocol v2
//!
//! The header is split into three 64-byte regions so producer-owned and
//! consumer-owned counters never share a cache line:
//!
//! - **config region** (offset 0): magic, version, sample rate, channels,
//!   capacity. Written once at initialization (magic last, with Release) and
//!   read-only afterwards.
//! - **producer region** (offset 64): `write_idx`, `overrun_count`, plus
//!   `producer_generation` / `producer_attach_count` reserved for app-owned
//!   producer health tracking.
//! - **consumer region** (offset 128): `read_idx`, `underrun_count`.
//!
//! All counters are accessed through atomics mapped over the shared region —
//! every offset is 8-byte aligned on a page-aligned mapping. Field ownership
//! is strict: the producer is the only plain writer of `write_idx` (Release,
//! published after the frame data) and the consumer is the only plain writer
//! of `underrun_count`. `read_idx` is advanced by the consumer **and** by the
//! producer when it must reclaim space (overwrite-oldest live semantics);
//! both sides use compare-exchange so a concurrent advance is never lost.
//! The whole-header read-modify-write of protocol v1 — which raced across
//! processes because the `Mutex` in [`SharedRingHandle`] is process-local —
//! is gone.
//!
//! Sample data is copied in at most two contiguous segments around the wrap
//! point. Audio frames may be overwritten while a lagging consumer copies
//! them (detected by its `read_idx` compare-exchange, which triggers a
//! bounded retry); this trades a rare transient artifact for a wait-free
//! producer, matching the drop-oldest policy of the v1 ring.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use dashmap::DashMap;
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap, shm_open, shm_unlink};
use nix::sys::stat::{Mode, fstat, umask};
use nix::unistd::ftruncate;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Header magic (`MARS`).
pub const RING_MAGIC: u32 = 0x4D_41_52_53;
/// Header schema version.
pub const RING_VERSION: u32 = 2;

/// Total header size: three cache-line-sized regions (config / producer /
/// consumer). Sample data starts at this offset, which keeps it 64-byte
/// aligned.
const HEADER_SIZE: usize = 192;

// Config region (written once at init).
const OFFSET_MAGIC: usize = 0;
const OFFSET_VERSION: usize = 4;
const OFFSET_SAMPLE_RATE: usize = 8;
const OFFSET_CHANNELS: usize = 12;
const OFFSET_CAPACITY: usize = 16;

// Producer-owned region.
const OFFSET_WRITE_IDX: usize = 64;
const OFFSET_OVERRUN: usize = 72;
const OFFSET_PRODUCER_GENERATION: usize = 80;
const OFFSET_PRODUCER_ATTACH: usize = 88;

// Consumer-owned region (`read_idx` is CAS-shared with the producer for
// overwrite-oldest space reclamation).
const OFFSET_READ_IDX: usize = 128;
const OFFSET_UNDERRUN: usize = 136;

/// Bounded retries for a consumer copy invalidated by a concurrent
/// producer space reclamation.
const READ_RETRY_LIMIT: usize = 3;

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

/// Snapshot of the shared ring header (diagnostic view; counters are read
/// with relaxed atomics).
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
    /// Producer attach counter (bumped when an external producer attaches).
    pub producer_attach_count: u64,
    /// Producer generation (bumped on attach and detach).
    pub producer_generation: u64,
}

/// Result of a ring transfer, including the xrun deltas attributable to this
/// call so callers never re-read the shared header for stat accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingTransfer {
    /// Frames actually transferred.
    pub frames: usize,
    /// Frames dropped/overwritten by this write call.
    pub overruns: u64,
    /// Underrun events caused by this read call (0 or 1).
    pub underruns: u64,
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
        let expected_len = spec.total_size_bytes();
        let (fd, created) = open_shm_fd(&shm_name)?;

        if created {
            return Self::init_fresh(shm_name, fd, spec);
        }

        // Existing object: it must be large enough for this spec (macOS
        // rounds shm object sizes up to page granularity, so an exact match
        // cannot be required) and carry a valid v2 header. Protocol v1
        // objects and spec mismatches fail the header check and are
        // recreated rather than silently reinterpreted.
        let size_ok = fstat(&fd)
            .map(|st| st.st_size >= expected_len as i64)
            .unwrap_or(false);

        if size_ok {
            if let Ok(map) = ShmMap::new(&fd, expected_len) {
                let ring = Self {
                    shm_name: shm_name.clone(),
                    fd,
                    map,
                    spec,
                };
                if ring.validate_header() {
                    return Ok(ring);
                }
            }
        }

        // Incompatible object (stale version, wrong spec, or corrupted
        // header): unlink and create a fresh one. Peers holding the old
        // mapping keep their private copy and re-open on their next
        // (re)configuration.
        let _ = shm_unlink(shm_name.as_str());
        let (fd, created) = open_shm_fd(&shm_name)?;
        if !created {
            return Err(RingError::Shm(format!(
                "failed to recreate incompatible shm object {shm_name}"
            )));
        }
        Self::init_fresh(shm_name, fd, spec)
    }

    fn init_fresh(shm_name: String, fd: OwnedFd, spec: RingSpec) -> Result<Self, RingError> {
        let expected_len = spec.total_size_bytes();
        ftruncate(&fd, expected_len as i64)
            .map_err(|error| RingError::Shm(format!("ftruncate failed: {error}")))?;

        let map = ShmMap::new(&fd, expected_len)?;
        let ring = Self {
            shm_name,
            fd,
            map,
            spec,
        };

        // ftruncate zero-fills, so all counters start at zero. Publish the
        // config fields, then the magic last with Release so openers that
        // observe the magic (Acquire) also observe a fully initialized
        // header.
        ring.atomic_u32(OFFSET_VERSION)
            .store(RING_VERSION, Ordering::Relaxed);
        ring.atomic_u32(OFFSET_SAMPLE_RATE)
            .store(spec.sample_rate, Ordering::Relaxed);
        ring.atomic_u32(OFFSET_CHANNELS)
            .store(u32::from(spec.channels), Ordering::Relaxed);
        ring.atomic_u32(OFFSET_CAPACITY)
            .store(spec.capacity_frames, Ordering::Relaxed);
        ring.atomic_u32(OFFSET_MAGIC)
            .store(RING_MAGIC, Ordering::Release);

        Ok(ring)
    }

    fn validate_header(&self) -> bool {
        self.atomic_u32(OFFSET_MAGIC).load(Ordering::Acquire) == RING_MAGIC
            && self.atomic_u32(OFFSET_VERSION).load(Ordering::Relaxed) == RING_VERSION
            && self.atomic_u32(OFFSET_SAMPLE_RATE).load(Ordering::Relaxed) == self.spec.sample_rate
            && self.atomic_u32(OFFSET_CHANNELS).load(Ordering::Relaxed)
                == u32::from(self.spec.channels)
            && self.atomic_u32(OFFSET_CAPACITY).load(Ordering::Relaxed) == self.spec.capacity_frames
    }

    #[inline]
    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        debug_assert!(offset + 4 <= HEADER_SIZE && offset.is_multiple_of(4));
        // SAFETY: the mapping is page-aligned and at least HEADER_SIZE bytes;
        // `offset` is in-bounds and 4-byte aligned, and all cross-process
        // header access goes through atomics.
        unsafe { &*(self.map.ptr.as_ptr().add(offset).cast::<AtomicU32>()) }
    }

    #[inline]
    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        debug_assert!(offset + 8 <= HEADER_SIZE && offset.is_multiple_of(8));
        // SAFETY: the mapping is page-aligned and at least HEADER_SIZE bytes;
        // `offset` is in-bounds and 8-byte aligned, and all cross-process
        // header access goes through atomics.
        unsafe { &*(self.map.ptr.as_ptr().add(offset).cast::<AtomicU64>()) }
    }

    #[inline]
    fn data_ptr(&self) -> *mut f32 {
        // SAFETY: HEADER_SIZE is within the mapping and 4-byte aligned.
        unsafe { self.map.ptr.as_ptr().add(HEADER_SIZE).cast::<f32>() }
    }

    /// Read a diagnostic snapshot of the ring header.
    pub fn header(&self) -> Result<RingHeader, RingError> {
        Ok(RingHeader {
            magic: self.atomic_u32(OFFSET_MAGIC).load(Ordering::Relaxed),
            version: self.atomic_u32(OFFSET_VERSION).load(Ordering::Relaxed),
            sample_rate: self.atomic_u32(OFFSET_SAMPLE_RATE).load(Ordering::Relaxed),
            channels: self.atomic_u32(OFFSET_CHANNELS).load(Ordering::Relaxed) as u16,
            capacity_frames: self.atomic_u32(OFFSET_CAPACITY).load(Ordering::Relaxed),
            write_idx: self.atomic_u64(OFFSET_WRITE_IDX).load(Ordering::Relaxed),
            read_idx: self.atomic_u64(OFFSET_READ_IDX).load(Ordering::Relaxed),
            overrun_count: self.atomic_u64(OFFSET_OVERRUN).load(Ordering::Relaxed),
            underrun_count: self.atomic_u64(OFFSET_UNDERRUN).load(Ordering::Relaxed),
            producer_attach_count: self
                .atomic_u64(OFFSET_PRODUCER_ATTACH)
                .load(Ordering::Relaxed),
            producer_generation: self
                .atomic_u64(OFFSET_PRODUCER_GENERATION)
                .load(Ordering::Relaxed),
        })
    }

    /// Record an external producer attaching to this ring.
    ///
    /// Returns the new attach count. Status paths use the generation/attach
    /// counters to distinguish absent from stale producers.
    pub fn attach_producer(&self) -> u64 {
        self.atomic_u64(OFFSET_PRODUCER_GENERATION)
            .fetch_add(1, Ordering::Relaxed);
        self.atomic_u64(OFFSET_PRODUCER_ATTACH)
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    /// Record an external producer detaching from this ring.
    pub fn detach_producer(&self) {
        self.atomic_u64(OFFSET_PRODUCER_GENERATION)
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Write interleaved frames into the ring (producer side).
    ///
    /// Live drop-oldest semantics: when the ring is full the producer
    /// reclaims space by advancing `read_idx` with a compare-exchange,
    /// counting each overwritten frame as an overrun. The new `write_idx` is
    /// published with Release only after the frame data is in place.
    pub fn write_interleaved(&mut self, interleaved: &[f32]) -> Result<RingTransfer, RingError> {
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

        let capacity = u64::from(self.spec.capacity_frames);
        let total_frames = (interleaved.len() / channels) as u64;
        if total_frames == 0 {
            return Ok(RingTransfer::default());
        }

        // Degenerate oversized write: only the last `capacity` frames can
        // survive; everything before them is dropped unwritten.
        let mut overruns = 0_u64;
        let src = if total_frames > capacity {
            overruns += total_frames - capacity;
            &interleaved[((total_frames - capacity) as usize) * channels..]
        } else {
            interleaved
        };
        let frames = (src.len() / channels) as u64;

        let write_idx = self.atomic_u64(OFFSET_WRITE_IDX).load(Ordering::Relaxed);

        // Reclaim space from the consumer if needed (overwrite-oldest).
        let read_atomic = self.atomic_u64(OFFSET_READ_IDX);
        let mut read_idx = read_atomic.load(Ordering::Acquire);
        loop {
            let used = write_idx.wrapping_sub(read_idx);
            let free = capacity.saturating_sub(used);
            if frames <= free {
                break;
            }
            let advance_to = write_idx.wrapping_add(frames).wrapping_sub(capacity);
            match read_atomic.compare_exchange(
                read_idx,
                advance_to,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    overruns += advance_to.wrapping_sub(read_idx);
                    break;
                }
                Err(actual) => read_idx = actual,
            }
        }

        self.copy_frames_in(write_idx, src, channels);

        // Publish the data before the new write index becomes visible.
        self.atomic_u64(OFFSET_WRITE_IDX)
            .store(write_idx.wrapping_add(frames), Ordering::Release);
        if overruns > 0 {
            self.atomic_u64(OFFSET_OVERRUN)
                .fetch_add(overruns, Ordering::Relaxed);
        }

        Ok(RingTransfer {
            frames: frames as usize,
            overruns,
            underruns: 0,
        })
    }

    /// Read interleaved frames from the ring (consumer side). Missing frames
    /// are zero-filled.
    ///
    /// The consumer copies first and then publishes its advance with a
    /// compare-exchange; if the producer reclaimed space mid-copy the
    /// exchange fails, the (possibly torn) copy is discarded, and the read
    /// retries from the producer-advanced position.
    pub fn read_interleaved(&mut self, out: &mut [f32]) -> Result<RingTransfer, RingError> {
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

        let requested = (out.len() / channels) as u64;
        if requested == 0 {
            return Ok(RingTransfer::default());
        }

        let read_atomic = self.atomic_u64(OFFSET_READ_IDX);
        let mut frames_read = 0_u64;
        for _ in 0..READ_RETRY_LIMIT {
            let read_idx = read_atomic.load(Ordering::Acquire);
            let write_idx = self.atomic_u64(OFFSET_WRITE_IDX).load(Ordering::Acquire);
            let available = write_idx.wrapping_sub(read_idx).min(requested);
            if available == 0 {
                break;
            }

            self.copy_frames_out(
                read_idx,
                &mut out[..(available as usize) * channels],
                channels,
            );

            match read_atomic.compare_exchange(
                read_idx,
                read_idx.wrapping_add(available),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    frames_read = available;
                    break;
                }
                // Producer reclaimed space mid-copy: discard and retry.
                Err(_) => continue,
            }
        }

        let mut underruns = 0_u64;
        if frames_read < requested {
            out[(frames_read as usize) * channels..].fill(0.0);
            self.atomic_u64(OFFSET_UNDERRUN)
                .fetch_add(1, Ordering::Relaxed);
            underruns = 1;
        }

        Ok(RingTransfer {
            frames: frames_read as usize,
            overruns: 0,
            underruns,
        })
    }

    /// Copy interleaved frames into the ring in at most two contiguous
    /// segments around the wrap point.
    fn copy_frames_in(&mut self, start_idx: u64, src: &[f32], channels: usize) {
        let capacity = self.spec.capacity_frames as usize;
        if capacity == 0 {
            return;
        }
        let frames = src.len() / channels;
        let slot = (start_idx % capacity as u64) as usize;
        let first = frames.min(capacity - slot);
        let data = self.data_ptr();
        // SAFETY: `slot + first <= capacity` and the spillover `frames -
        // first <= capacity` fit the mapped data region; `src` holds exactly
        // `frames * channels` samples. The destination may be concurrently
        // read by a lagging consumer in another process; the consumer detects
        // that via its read_idx compare-exchange and discards the torn copy.
        unsafe {
            std::ptr::copy_nonoverlapping(
                src.as_ptr(),
                data.add(slot * channels),
                first * channels,
            );
            if frames > first {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(first * channels),
                    data,
                    (frames - first) * channels,
                );
            }
        }
    }

    /// Copy interleaved frames out of the ring in at most two contiguous
    /// segments around the wrap point.
    fn copy_frames_out(&self, start_idx: u64, out: &mut [f32], channels: usize) {
        let capacity = self.spec.capacity_frames as usize;
        if capacity == 0 {
            return;
        }
        let frames = out.len() / channels;
        let slot = (start_idx % capacity as u64) as usize;
        let first = frames.min(capacity - slot);
        let data = self.data_ptr();
        // SAFETY: bounds as in `copy_frames_in`; the source may be
        // concurrently overwritten by the producer, which the caller detects
        // through the read_idx compare-exchange and retries.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.add(slot * channels),
                out.as_mut_ptr(),
                first * channels,
            );
            if frames > first {
                std::ptr::copy_nonoverlapping(
                    data,
                    out.as_mut_ptr().add(first * channels),
                    (frames - first) * channels,
                );
            }
        }
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

/// Build a stream name from direction + uid + capability token.
///
/// Ring objects are created with cross-user permissions so the HAL plug-in
/// inside coreaudiod can open them; the unguessable token suffix is what
/// gates access (POSIX SHM has no ACLs on macOS). The token is distributed
/// only over trusted channels: the DesiredState property to the HAL and the
/// per-user IPC socket to SDK clients. An empty token yields the legacy
/// untagged name.
#[must_use]
pub fn stream_name_tagged(direction: StreamDirection, uid: &str, token: &str) -> String {
    let base = stream_name(direction, uid);
    if token.is_empty() {
        base
    } else {
        format!("{base}.{token}")
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

/// Map a logical ring name to its POSIX SHM object name.
///
/// macOS limits POSIX SHM names to 31 bytes including the leading slash
/// (`PSHMNAMLEN`). Logical names — `mars.vout.<uid>[.<token>]` — routinely
/// exceed that (the previous scheme silently failed with ENAMETOOLONG for
/// longer device uids), so the object name is a fixed-length digest of the
/// logical name: `/mars.<fnv1a64 hex>` (22 bytes). Both the daemon and the
/// HAL derive it from the same logical name, and a capability token in the
/// logical name makes the digest unguessable to processes that never learned
/// the token.
fn to_posix_shm_name(public_name: &str) -> Result<String, RingError> {
    if public_name.is_empty() {
        return Err(RingError::InvalidName(public_name.to_string()));
    }

    Ok(format!("/mars.{:016x}", fnv1a64(public_name.as_bytes())))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Permission bits applied to newly created ring objects.
///
/// Rings cross user boundaries: the daemon/app runs as the logged-in user
/// while the HAL plug-in runs inside coreaudiod (`_coreaudiod`), so 0o600
/// objects are unreadable on the real audio path even though same-user unit
/// tests pass. The default is world-rw — access control comes from the
/// unguessable capability token in the ring name (see [`stream_name_tagged`]).
/// Override with `MARS_SHM_MODE` (octal) for locked-down single-user setups.
fn ring_mode_bits() -> u32 {
    std::env::var("MARS_SHM_MODE")
        .ok()
        .and_then(|raw| u32::from_str_radix(raw.trim_start_matches("0o"), 8).ok())
        .unwrap_or(0o666)
}

/// Serializes umask manipulation during ring creation.
static UMASK_GUARD: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn open_shm_fd(shm_name: &str) -> Result<(OwnedFd, bool), RingError> {
    let mode_bits = ring_mode_bits();
    let mode = Mode::from_bits_truncate(mode_bits as nix::libc::mode_t);
    let create_flags = OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR;

    // shm_open filters the mode through the process umask and macOS rejects
    // fchmod on shm descriptors (EINVAL), so the only way to create the
    // object with deterministic cross-user bits is to clear the umask for
    // the duration of the call. The guard serializes creators within this
    // process and keeps the window to the single shm_open syscall.
    let created = {
        let _guard = UMASK_GUARD.lock();
        let previous = umask(Mode::empty());
        let result = shm_open(shm_name, create_flags, mode);
        let _ = umask(previous);
        result
    };

    match created {
        Ok(fd) => Ok((fd, true)),
        Err(Errno::EEXIST) => shm_open(shm_name, OFlag::O_RDWR, Mode::empty())
            .map(|fd| (fd, false))
            .map_err(|error| RingError::Shm(format!("shm_open {shm_name}: {error}"))),
        Err(error) => Err(RingError::Shm(format!("shm_open {shm_name}: {error}"))),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
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
            let transfer = writer
                .write_interleaved(&[0.1, 0.2, 0.3, 0.4])
                .expect("write works");
            assert_eq!(transfer.frames, 2);
            assert_eq!(transfer.overruns, 0);
        }

        {
            let mut out = [0.0_f32; 4];
            let mut reader = reader.lock();
            let got = reader.read_interleaved(&mut out).expect("read works");
            assert_eq!(got.frames, 2);
            assert_eq!(got.underruns, 0);
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
            let transfer = guard
                .write_interleaved(&[1.0, 1.0, 2.0, 2.0, 3.0, 3.0])
                .expect("write should succeed");
            assert!(transfer.overruns >= 1);
            assert!(guard.header().expect("header").overrun_count >= 1);

            let mut out = [0.0_f32; 6];
            let read = guard
                .read_interleaved(&mut out)
                .expect("read should succeed");
            assert_eq!(read.frames, 2);
            assert_eq!(read.underruns, 1);
            assert!(guard.header().expect("header").underrun_count >= 1);
        }

        let _ = global_registry().remove(&name);
    }

    #[test]
    fn oversized_write_keeps_latest_frames() {
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 1,
            capacity_frames: 4,
        };
        let name = stream_name(StreamDirection::Vin, "oversized");
        let registry = RingRegistry::default();
        let ring = registry.create_or_open(&name, spec).expect("create ring");

        {
            let mut guard = ring.lock();
            let transfer = guard
                .write_interleaved(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
                .expect("write works");
            assert_eq!(transfer.frames, 4);
            assert_eq!(transfer.overruns, 2);

            let mut out = [0.0_f32; 4];
            let read = guard.read_interleaved(&mut out).expect("read works");
            assert_eq!(read.frames, 4);
            assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
        }

        let _ = registry.remove(&name);
    }

    #[test]
    fn wraparound_preserves_frame_order() {
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 2,
            capacity_frames: 4,
        };
        let name = stream_name(StreamDirection::Vin, "wraparound");
        let registry = RingRegistry::default();
        let ring = registry.create_or_open(&name, spec).expect("create ring");

        {
            let mut guard = ring.lock();
            // Fill, drain half, then write across the wrap point.
            guard
                .write_interleaved(&[1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0])
                .expect("fill");
            let mut half = [0.0_f32; 4];
            let read = guard.read_interleaved(&mut half).expect("drain half");
            assert_eq!(read.frames, 2);
            assert_eq!(half, [1.0, 1.0, 2.0, 2.0]);

            guard
                .write_interleaved(&[5.0, 5.0, 6.0, 6.0])
                .expect("wrap write");

            let mut out = [0.0_f32; 8];
            let read = guard.read_interleaved(&mut out).expect("read all");
            assert_eq!(read.frames, 4);
            assert_eq!(out, [3.0, 3.0, 4.0, 4.0, 5.0, 5.0, 6.0, 6.0]);
        }

        let _ = registry.remove(&name);
    }

    #[test]
    fn created_rings_are_cross_user_readable_and_writable() {
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 1,
            capacity_frames: 4,
        };
        let name = stream_name(StreamDirection::Vin, "mode-bits");
        let registry = RingRegistry::default();
        let ring = registry.create_or_open(&name, spec).expect("create ring");

        {
            let guard = ring.lock();
            let st = nix::sys::stat::fstat(guard._fd()).expect("fstat ring fd");
            let mode = u32::from(st.st_mode) & 0o777;
            assert_eq!(
                mode, 0o666,
                "ring objects must be cross-user accessible (got {mode:o})"
            );
        }

        let _ = registry.remove(&name);
    }

    #[test]
    fn tagged_stream_names_append_capability_token() {
        use super::stream_name_tagged;
        assert_eq!(
            stream_name_tagged(StreamDirection::Vin, "uid", "abc123"),
            "mars.vin.uid.abc123"
        );
        assert_eq!(
            stream_name_tagged(StreamDirection::Vout, "uid", ""),
            "mars.vout.uid"
        );
    }

    #[test]
    fn posix_names_fit_macos_pshmnamlen_for_any_logical_name() {
        // macOS rejects POSIX SHM names longer than 31 bytes (incl. '/');
        // logical names of any length must map to a fixed-size digest.
        let long_uid = "com.mars.managed.vout.some-very-long-device-identifier";
        let logical =
            super::stream_name_tagged(StreamDirection::Vout, long_uid, "ff67b8c8d64a45e8");
        let posix = super::to_posix_shm_name(&logical).expect("digest name");
        assert!(posix.len() <= 31, "got {} bytes: {posix}", posix.len());
        assert!(posix.starts_with("/mars."));
        // Deterministic: both daemon and HAL must derive the same object name.
        assert_eq!(posix, super::to_posix_shm_name(&logical).expect("again"));
        // Token changes the object name entirely.
        let other = super::stream_name_tagged(StreamDirection::Vout, long_uid, "0000000000000000");
        assert_ne!(posix, super::to_posix_shm_name(&other).expect("other"));

        // A ring with a long logical name actually opens (the v1 scheme
        // failed here with ENAMETOOLONG and the failure was swallowed).
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 1,
            capacity_frames: 4,
        };
        let registry = RingRegistry::default();
        let ring = registry.create_or_open(&logical, spec);
        assert!(ring.is_ok(), "long logical names must open: {ring:?}");
        let _ = registry.remove(&logical);
    }

    #[test]
    fn producer_attach_counters_are_tracked() {
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 1,
            capacity_frames: 4,
        };
        let name = stream_name(StreamDirection::Vin, "attach");
        let registry = RingRegistry::default();
        let ring = registry.create_or_open(&name, spec).expect("create ring");

        {
            let guard = ring.lock();
            assert_eq!(guard.header().expect("header").producer_attach_count, 0);
            assert_eq!(guard.attach_producer(), 1);
            let header = guard.header().expect("header");
            assert_eq!(header.producer_attach_count, 1);
            assert_eq!(header.producer_generation, 1);
            guard.detach_producer();
            assert_eq!(guard.header().expect("header").producer_generation, 2);
        }

        let _ = registry.remove(&name);
    }

    #[test]
    fn concurrent_producer_consumer_never_loses_indices() {
        // Regression for the v1 whole-header read-modify-write race: a
        // producer and consumer hammering the same ring from two threads
        // (sharing the mmap like two processes would) must end with
        // consistent monotonic indices.
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 1,
            capacity_frames: 64,
        };
        let name = stream_name(StreamDirection::Vin, "stress");
        let registry_a = RingRegistry::default();
        let registry_b = RingRegistry::default();
        let producer_ring = registry_a.create_or_open(&name, spec).expect("producer");
        let consumer_ring = registry_b.create_or_open(&name, spec).expect("consumer");

        const ROUNDS: usize = 10_000;
        let producer = std::thread::spawn(move || {
            let chunk = [1.0_f32; 16];
            for _ in 0..ROUNDS {
                let mut guard = producer_ring.lock();
                let _ = guard.write_interleaved(&chunk).expect("write");
            }
        });
        let consumer = std::thread::spawn(move || {
            let mut out = [0.0_f32; 16];
            let mut frames = 0_u64;
            for _ in 0..ROUNDS {
                let mut guard = consumer_ring.lock();
                let transfer = guard.read_interleaved(&mut out).expect("read");
                frames += transfer.frames as u64;
            }
            frames
        });

        producer.join().expect("producer thread");
        let _consumed = consumer.join().expect("consumer thread");

        let verify = registry_a.create_or_open(&name, spec).expect("verify");
        let header = verify.lock().header().expect("header");
        let written = ROUNDS as u64 * 16;
        assert_eq!(header.write_idx, written);
        // read_idx can never exceed write_idx nor lag more than capacity.
        assert!(header.read_idx <= header.write_idx);
        assert!(header.write_idx - header.read_idx <= u64::from(spec.capacity_frames));

        let _ = registry_a.remove(&name);
    }
}

//! Downstream virtual microphone data plane: [`VirtualMic`] and
//! [`LiveWriter`].
//!
//! Apps never construct ring names or touch MARS internals: the daemon's
//! `EnsureVirtualInput` response carries everything needed to attach, and
//! the writer wraps the shared-memory ring v2 producer half.
//!
//! # Example
//!
//! ```no_run
//! # async fn demo() -> Result<(), mars_sdk::MarsClientError> {
//! use mars_sdk::{AppVirtualInput, MarsClient, ProducerKind};
//!
//! let client = MarsClient::new_default(MarsClient::default_timeout())?;
//! let mic = client
//!     .ensure_virtual_input(AppVirtualInput {
//!         app_id: "com.example.virtual-mic-app".into(),
//!         id: "primary-mic".into(),
//!         name: "Virtual Mic".into(),
//!         uid: "com.example.virtual-mic-app.primary-mic".into(),
//!         sample_rate: 48_000,
//!         channels: 1,
//!         producer: ProducerKind::ExternalApp,
//!     })
//!     .await?;
//!
//! let mut writer = mic.open_live_writer()?;
//! let frames = vec![0.0_f32; 480]; // 10 ms of mono 48 kHz audio
//! writer.write_f32_interleaved_live(&frames)?;
//! writer.flush_silence()?;
//! # Ok(())
//! # }
//! ```

use mars_shm::{RingSpec, SharedRingHandle, global_registry};
use mars_types::EnsuredVirtualInput;

use crate::MarsClientError;

/// Handle to an ensured app-owned virtual input device.
#[derive(Debug, Clone)]
pub struct VirtualMic {
    ensured: EnsuredVirtualInput,
}

impl VirtualMic {
    pub(crate) fn new(ensured: EnsuredVirtualInput) -> Self {
        Self { ensured }
    }

    /// Device uid (what CoreAudio clients see and remember).
    #[must_use]
    pub fn uid(&self) -> &str {
        &self.ensured.uid
    }

    /// Full ensure response, including producer health at ensure time.
    #[must_use]
    pub const fn info(&self) -> &EnsuredVirtualInput {
        &self.ensured
    }

    /// Open the sole live audio writer for this input.
    ///
    /// Attaches to the shared ring (bumping the producer attach counter that
    /// `mars status` reports) and returns a writer whose hot path is
    /// realtime-safe: no allocation, no blocking locks shared with other
    /// processes, bulk two-segment copies.
    pub fn open_live_writer(&self) -> Result<LiveWriter, MarsClientError> {
        let spec = RingSpec {
            sample_rate: self.ensured.sample_rate,
            channels: self.ensured.channels,
            capacity_frames: self.ensured.capacity_frames,
        };
        let ring = global_registry()
            .create_or_open(&self.ensured.ring_name, spec)
            .map_err(|error| MarsClientError::RingAttachFailed(error.to_string()))?;
        ring.lock().attach_producer();
        Ok(LiveWriter {
            ring,
            channels: usize::from(self.ensured.channels),
            silence_frames: self.ensured.capacity_frames as usize,
        })
    }
}

/// Live audio writer for an app-owned virtual input.
///
/// Live semantics: writes never block and never wait for the consumer — when
/// the ring is full the oldest frames are dropped so latency stays bounded.
/// Detaches from the ring on drop (bumping the producer generation so
/// `mars status` can distinguish detached from stalled producers).
#[derive(Debug)]
pub struct LiveWriter {
    ring: SharedRingHandle,
    channels: usize,
    silence_frames: usize,
}

impl LiveWriter {
    /// Write interleaved Float32 frames.
    ///
    /// Returns the number of frames written. Safe to call from the app's
    /// audio callback: the underlying ring write is allocation-free and
    /// wait-free against the consumer.
    pub fn write_f32_interleaved_live(&mut self, frames: &[f32]) -> Result<usize, MarsClientError> {
        if !frames.len().is_multiple_of(self.channels.max(1)) {
            return Err(MarsClientError::SampleAlignment {
                expected: self.channels,
                actual: frames.len(),
            });
        }
        let transfer = self
            .ring
            .lock()
            .write_interleaved(frames)
            .map_err(|error| MarsClientError::RingAttachFailed(error.to_string()))?;
        Ok(transfer.frames)
    }

    /// Drop all frames the consumer has not read yet.
    ///
    /// Use on mode changes so the next read starts at fresh audio. Returns
    /// the number of frames dropped.
    pub fn drop_backlog(&mut self) -> u64 {
        self.ring.lock().drop_backlog()
    }

    /// Alias for [`Self::drop_backlog`] matching the issue #40 API sketch.
    pub fn clear_unread(&mut self) -> u64 {
        self.drop_backlog()
    }

    /// Write one ring's worth of silence so the consumer decays smoothly to
    /// zero on shutdown or mode changes.
    pub fn flush_silence(&mut self) -> Result<(), MarsClientError> {
        let zeros = vec![0.0_f32; self.silence_frames * self.channels.max(1)];
        self.write_f32_interleaved_live(&zeros).map(|_| ())
    }
}

impl Drop for LiveWriter {
    fn drop(&mut self) {
        self.ring.lock().detach_producer();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use mars_types::{ProducerKind, ProducerState, VirtualInputProducerStatus};

    use super::*;

    fn test_ensured(name: &str) -> EnsuredVirtualInput {
        EnsuredVirtualInput {
            uid: format!("test.{name}"),
            ring_name: format!("mars.vin.test.{name}.deadbeef00000000"),
            sample_rate: 48_000,
            channels: 1,
            capacity_frames: 64,
            producer: VirtualInputProducerStatus {
                id: name.to_string(),
                uid: format!("test.{name}"),
                kind: ProducerKind::ExternalApp,
                state: ProducerState::Absent,
                write_idx: 0,
                underrun_count: 0,
                attach_count: 0,
                generation: 0,
            },
        }
    }

    #[test]
    fn live_writer_attach_write_clear_flush_detach() {
        let mic = VirtualMic::new(test_ensured("writer-cycle"));
        let ring_name = mic.info().ring_name.clone();

        let mut writer = mic.open_live_writer().expect("open writer");

        // Attach is visible in the shared header.
        let ring = global_registry().open(&ring_name).expect("ring exists");
        assert_eq!(
            ring.lock().header().expect("header").producer_attach_count,
            1
        );
        assert_eq!(ring.lock().header().expect("header").producer_generation, 1);

        let written = writer
            .write_f32_interleaved_live(&[0.25_f32; 32])
            .expect("write");
        assert_eq!(written, 32);

        assert_eq!(writer.clear_unread(), 32, "backlog cleared");

        writer.flush_silence().expect("flush");
        let header = ring.lock().header().expect("header");
        assert_eq!(header.write_idx, 32 + 64, "silence fills one ring");

        drop(writer);
        let header = ring.lock().header().expect("header");
        assert_eq!(header.producer_generation, 2, "detach bumps generation");

        let _ = global_registry().remove(&ring_name);
    }

    #[test]
    fn live_writer_rejects_misaligned_sample_counts() {
        let mut ensured = test_ensured("misaligned");
        ensured.channels = 2;
        ensured.ring_name = "mars.vin.test.misaligned.feedface00000000".to_string();
        let mic = VirtualMic::new(ensured);
        let mut writer = mic.open_live_writer().expect("open writer");

        let error = writer
            .write_f32_interleaved_live(&[0.0_f32; 3])
            .expect_err("odd sample count must fail for stereo");
        assert!(matches!(error, MarsClientError::SampleAlignment { .. }));

        let _ = global_registry().remove(&mic.info().ring_name.clone());
    }
}

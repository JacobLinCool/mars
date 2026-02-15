#![forbid(unsafe_code)]
//! Safe facade for MARS shared-memory ring buffers.
//!
//! Actual mmap + POSIX SHM implementation lives in `mars-hal::shm_backend` so
//! all unsafe code remains centralized in `mars-hal`.

pub use mars_hal::shm_backend::{
    RING_MAGIC, RING_VERSION, RingError, RingHeader, RingRegistry, RingSpec, SharedRing,
    SharedRingHandle, StreamDirection, global_registry, stream_name,
};

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{RingRegistry, RingSpec, StreamDirection, global_registry, stream_name};

    #[test]
    fn shared_between_independent_registries() {
        let spec = RingSpec {
            sample_rate: 48_000,
            channels: 2,
            capacity_frames: 8,
        };
        let name = stream_name(StreamDirection::Vout, "cross-process-facade");

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
        let name = stream_name(StreamDirection::Vout, "test-facade");
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

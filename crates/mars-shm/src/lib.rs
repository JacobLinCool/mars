#![forbid(unsafe_code)]
//! Safe facade for MARS shared-memory ring buffers.
//!
//! Actual mmap + POSIX SHM implementation lives in `mars-hal::shm_backend` so
//! all unsafe code remains centralized in `mars-hal`.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

pub use mars_hal::shm_backend::{
    RING_MAGIC, RING_VERSION, RingError, RingHeader, RingRegistry, RingSpec, RingTransfer,
    SharedRing, SharedRingHandle, StreamDirection, global_registry, stream_name,
    stream_name_tagged,
};

static TOKEN_CACHE: Lazy<Mutex<Option<HashMap<String, String>>>> = Lazy::new(|| Mutex::new(None));

fn ring_tokens_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("MARS_RING_TOKENS_PATH") {
        return Some(PathBuf::from(path));
    }
    dirs::home_dir().map(|home| home.join("Library/Application Support/mars/ring_tokens.json"))
}

fn load_tokens(path: &PathBuf) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
        .unwrap_or_default()
}

fn persist_tokens(path: &PathBuf, tokens: &HashMap<String, String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string_pretty(tokens).unwrap_or_else(|_| "{}".to_string());
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(payload.as_bytes())
}

/// Return the capability token for a device uid, generating and persisting a
/// new one on first use.
///
/// Tokens gate access to world-rw ring objects (POSIX SHM has no ACLs on
/// macOS): only processes that learn the full tagged ring name — the HAL via
/// the DesiredState property channel, SDK clients via the per-user IPC
/// socket — can open them. The token store lives in the user's Application
/// Support directory with mode 0600 and tokens are stable across daemon
/// restarts so unchanged devices are not churned on re-apply.
///
/// Returns an empty token (legacy untagged naming) when no home directory is
/// available; ring permissions still apply.
#[must_use]
pub fn ring_token_for(uid: &str) -> String {
    let Some(path) = ring_tokens_path() else {
        return String::new();
    };

    let mut cache = TOKEN_CACHE.lock();
    let tokens = cache.get_or_insert_with(|| load_tokens(&path));
    ring_token_in(&path, tokens, uid)
}

fn ring_token_in(path: &PathBuf, tokens: &mut HashMap<String, String>, uid: &str) -> String {
    if let Some(existing) = tokens.get(uid) {
        return existing.clone();
    }

    let token = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
    tokens.insert(uid.to_string(), token.clone());
    if let Err(error) = persist_tokens(path, tokens) {
        // Persist failures degrade to per-run tokens (devices churn on the
        // next daemon restart) but never to silent token reuse.
        eprintln!("mars-shm: failed to persist ring tokens to {path:?}: {error}");
    }
    token
}

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
            assert_eq!(got.frames, 2);
            assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
        }

        let _ = registry_a.remove(&name);
        let _ = registry_b.remove(&name);
    }

    #[test]
    fn ring_tokens_are_stable_and_unguessable() {
        let dir = std::env::temp_dir().join(format!("mars-token-test-{}", std::process::id()));
        let path = dir.join("ring_tokens.json");
        let mut tokens = std::collections::HashMap::new();

        let first = super::ring_token_in(&path, &mut tokens, "token-test-uid");
        let second = super::ring_token_in(&path, &mut tokens, "token-test-uid");
        let other = super::ring_token_in(&path, &mut tokens, "token-test-other");

        assert_eq!(first, second, "tokens must be stable per uid");
        assert_ne!(first, other, "tokens must differ per uid");
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(path.exists(), "token store must be persisted");

        // A fresh load from disk yields the same tokens (restart stability).
        let reloaded = super::load_tokens(&path);
        assert_eq!(reloaded.get("token-test-uid"), Some(&first));

        let _ = std::fs::remove_dir_all(&dir);
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

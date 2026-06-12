//! Embeddable runtime package management for MARS.
//!
//! This module defines the versioned runtime package manifest and a read-only
//! [`runtime_status`] probe that downstream apps (Tauri/native installers) can
//! poll without ever blocking on CoreAudio enumeration: every check is either
//! a plain filesystem read or an IPC call with an explicit timeout.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mars_ipc::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};

use crate::{MarsClient, MarsClientError};

/// Directory that holds the `mars` and `marsd` binaries.
pub const DEFAULT_BIN_DIR: &str = "/usr/local/bin";
/// Installed HAL driver bundle location.
pub const DEFAULT_DRIVER_BUNDLE_PATH: &str = "/Library/Audio/Plug-Ins/HAL/mars.driver";
/// launchd label of the per-user daemon agent.
pub const LAUNCH_AGENT_LABEL: &str = "com.mars.marsd";
/// Per-user LaunchAgent plist path relative to `$HOME`.
pub const LAUNCH_AGENT_PATH_RELATIVE: &str = "Library/LaunchAgents/com.mars.marsd.plist";
/// Per-user install receipt (copy of the installed package manifest),
/// relative to `$HOME`.
pub const RECEIPT_PATH_RELATIVE: &str = "Library/Application Support/mars/runtime-manifest.json";

const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_DOCTOR_TIMEOUT: Duration = Duration::from_secs(4);

/// Manifest of a versioned runtime package (`mars-runtime-<version>.tar.gz`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeManifest {
    /// Runtime version (matches the daemon crate version).
    pub version: String,
    /// Minimum supported macOS version, e.g. `"15.0"`.
    pub min_macos: String,
    /// IPC protocol version the packaged daemon speaks.
    pub protocol_version: u16,
    /// Files contained in the package, relative to the package root.
    pub files: Vec<ManifestFile>,
}

/// A single file entry in [`RuntimeManifest::files`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    /// Path relative to the package root, e.g. `bin/marsd`.
    pub path: String,
    /// Hex-encoded SHA-256 digest of the file contents.
    pub sha256: String,
    /// Expected code-signing authority (`codesign` `Authority=` value).
    /// `None` for unsigned/non-code artifacts.
    #[serde(default)]
    pub codesign_id: Option<String>,
}

/// High-level installation state of the MARS runtime on this machine.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    /// One or more required components (binaries, driver bundle) are absent.
    Missing,
    /// All components are installed but the daemon does not answer pings.
    InstalledNotRunning,
    /// Daemon responds and all observed versions agree.
    Healthy,
    /// Daemon responds but the running version lags the installed files
    /// (restart/upgrade required).
    Stale,
    /// Protocol or major-version mismatch between SDK, daemon, and driver.
    Incompatible,
}

/// Read-only snapshot of the installed runtime, suitable for app UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub state: RuntimeState,
    /// Version recorded by the installer receipt (if any).
    pub installed_version: Option<String>,
    /// Driver bundle version read from its `Info.plist` on disk.
    pub driver_version: Option<String>,
    /// Version reported by the running daemon (if reachable).
    pub daemon_version: Option<String>,
    /// IPC protocol version this SDK speaks.
    pub protocol_version: u16,
    /// IPC protocol version observed from the daemon, when it differs.
    pub daemon_protocol_version: Option<u16>,
    pub cli_binary_installed: bool,
    pub daemon_binary_installed: bool,
    pub driver_bundle_installed: bool,
    pub launch_agent_installed: bool,
    pub daemon_responding: bool,
}

/// Filesystem layout of an installed MARS runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLayout {
    /// `mars` CLI binary path.
    pub cli_path: PathBuf,
    /// `marsd` daemon binary path.
    pub daemon_path: PathBuf,
    /// HAL driver bundle directory.
    pub driver_bundle_path: PathBuf,
    /// Per-user LaunchAgent plist path.
    pub launch_agent_path: PathBuf,
    /// Per-user install receipt path.
    pub receipt_path: PathBuf,
    /// Daemon IPC socket path.
    pub socket_path: PathBuf,
}

impl RuntimeLayout {
    /// Standard layout for the current user.
    #[cfg(feature = "default-socket-path")]
    #[cfg_attr(docsrs, doc(cfg(feature = "default-socket-path")))]
    pub fn standard() -> Result<Self, MarsClientError> {
        let home = dirs::home_dir().ok_or(MarsClientError::HomeDirectoryUnavailable)?;
        Ok(Self::for_home(&home))
    }

    /// Standard layout rooted at an explicit home directory.
    #[must_use]
    pub fn for_home(home: &Path) -> Self {
        Self {
            cli_path: Path::new(DEFAULT_BIN_DIR).join("mars"),
            daemon_path: Path::new(DEFAULT_BIN_DIR).join("marsd"),
            driver_bundle_path: PathBuf::from(DEFAULT_DRIVER_BUNDLE_PATH),
            launch_agent_path: home.join(LAUNCH_AGENT_PATH_RELATIVE),
            receipt_path: home.join(RECEIPT_PATH_RELATIVE),
            socket_path: home.join(mars_types::DEFAULT_SOCKET_PATH_RELATIVE),
        }
    }
}

/// Timeouts for the read-only status probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusOptions {
    /// Timeout for the daemon liveness ping.
    pub ping_timeout: Duration,
    /// Timeout for the follow-up doctor request used to read the daemon
    /// version. Doctor enumeration on the daemon side is deadline-bounded,
    /// but this client-side timeout guarantees the probe never hangs even
    /// against older daemons.
    pub doctor_timeout: Duration,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            ping_timeout: DEFAULT_PING_TIMEOUT,
            doctor_timeout: DEFAULT_DOCTOR_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PingOutcome {
    Responding,
    ProtocolMismatch(Option<u16>),
    Unreachable,
}

/// Probe the installed runtime. Never blocks beyond the configured timeouts
/// and never mutates anything (it does not auto-launch the daemon).
pub async fn runtime_status(layout: &RuntimeLayout, options: &StatusOptions) -> RuntimeStatus {
    let cli_binary_installed = layout.cli_path.is_file();
    let daemon_binary_installed = layout.daemon_path.is_file();
    let driver_bundle_installed = layout.driver_bundle_path.is_dir();
    let launch_agent_installed = layout.launch_agent_path.is_file();

    let mut driver_version = driver_bundle_version(&layout.driver_bundle_path);
    let installed_version = read_receipt(&layout.receipt_path).map(|manifest| manifest.version);

    let components_present =
        cli_binary_installed && daemon_binary_installed && driver_bundle_installed;

    let ping_client = MarsClient::new(layout.socket_path.clone(), options.ping_timeout);
    let ping = match ping_client.ping().await {
        Ok(()) => PingOutcome::Responding,
        Err(MarsClientError::Ipc(mars_ipc::IpcError::ProtocolVersionMismatch {
            actual, ..
        })) => PingOutcome::ProtocolMismatch(Some(actual)),
        Err(MarsClientError::Ipc(mars_ipc::IpcError::DaemonError { message, .. }))
            if message.contains("protocol mismatch") =>
        {
            PingOutcome::ProtocolMismatch(None)
        }
        Err(_) => PingOutcome::Unreachable,
    };

    let mut daemon_version = None;
    if ping == PingOutcome::Responding {
        let doctor_client = MarsClient::new(layout.socket_path.clone(), options.doctor_timeout);
        if let Ok(report) = doctor_client.doctor().await {
            daemon_version = Some(report.daemon_version);
            if driver_version.is_none() {
                driver_version = report.driver_version;
            }
        }
    }

    let state = derive_state(
        components_present,
        ping,
        installed_version.as_deref(),
        driver_version.as_deref(),
        daemon_version.as_deref(),
    );

    RuntimeStatus {
        state,
        installed_version,
        driver_version,
        daemon_version,
        protocol_version: PROTOCOL_VERSION,
        daemon_protocol_version: match ping {
            PingOutcome::ProtocolMismatch(actual) => actual,
            PingOutcome::Responding | PingOutcome::Unreachable => None,
        },
        cli_binary_installed,
        daemon_binary_installed,
        driver_bundle_installed,
        launch_agent_installed,
        daemon_responding: ping == PingOutcome::Responding,
    }
}

fn derive_state(
    components_present: bool,
    ping: PingOutcome,
    installed_version: Option<&str>,
    driver_version: Option<&str>,
    daemon_version: Option<&str>,
) -> RuntimeState {
    if !components_present {
        return RuntimeState::Missing;
    }

    match ping {
        PingOutcome::Unreachable => RuntimeState::InstalledNotRunning,
        PingOutcome::ProtocolMismatch(_) => RuntimeState::Incompatible,
        PingOutcome::Responding => {
            if major_versions_conflict(driver_version, daemon_version) {
                return RuntimeState::Incompatible;
            }

            let stale_installed = matches!(
                (installed_version, daemon_version),
                (Some(installed), Some(daemon)) if installed != daemon
            );
            let stale_driver = matches!(
                (driver_version, daemon_version),
                (Some(driver), Some(daemon)) if driver != daemon
            );
            if stale_installed || stale_driver {
                RuntimeState::Stale
            } else {
                RuntimeState::Healthy
            }
        }
    }
}

fn major_versions_conflict(driver_version: Option<&str>, daemon_version: Option<&str>) -> bool {
    match (
        driver_version.and_then(parse_major),
        daemon_version.and_then(parse_major),
    ) {
        (Some(driver_major), Some(daemon_major)) => driver_major != daemon_major,
        _ => false,
    }
}

fn parse_major(version: &str) -> Option<u64> {
    version.split('.').next()?.trim().parse::<u64>().ok()
}

/// Read `CFBundleShortVersionString` from the driver bundle `Info.plist`.
#[must_use]
pub fn driver_bundle_version(driver_bundle_path: &Path) -> Option<String> {
    let info_plist = driver_bundle_path.join("Contents/Info.plist");
    let xml = std::fs::read_to_string(info_plist).ok()?;
    plist_string_value(&xml, "CFBundleShortVersionString")
        .or_else(|| plist_string_value(&xml, "CFBundleVersion"))
}

/// Read the per-user install receipt (a copy of the installed manifest).
#[must_use]
pub fn read_receipt(receipt_path: &Path) -> Option<RuntimeManifest> {
    let raw = std::fs::read_to_string(receipt_path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Minimal XML plist lookup: returns the `<string>` value that follows
/// `<key>{key}</key>`. Sufficient for the Info.plist files MARS ships.
fn plist_string_value(plist_xml: &str, key: &str) -> Option<String> {
    let key_tag = format!("<key>{key}</key>");
    let key_index = plist_xml.find(&key_tag)?;
    let after_key = &plist_xml[key_index + key_tag.len()..];
    let value_start = after_key.find("<string>")? + "<string>".len();
    let rest = &after_key[value_start..];
    let value_end = rest.find("</string>")?;
    Some(rest[..value_end].trim().to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SAMPLE_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>com.mars.driver</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>0.1.0</string>
</dict>
</plist>"#;

    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = RuntimeManifest {
            version: "0.1.0".to_string(),
            min_macos: "15.0".to_string(),
            protocol_version: PROTOCOL_VERSION,
            files: vec![ManifestFile {
                path: "bin/marsd".to_string(),
                sha256: "ab".repeat(32),
                codesign_id: Some("Developer ID Application: Example (TEAM)".to_string()),
            }],
        };

        let encoded = serde_json::to_string(&manifest).unwrap();
        let decoded: RuntimeManifest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn manifest_codesign_id_defaults_to_none() {
        let decoded: ManifestFile =
            serde_json::from_str(r#"{"path":"launchd/com.mars.marsd.plist","sha256":"00"}"#)
                .unwrap();
        assert_eq!(decoded.codesign_id, None);
    }

    #[test]
    fn runtime_state_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&RuntimeState::InstalledNotRunning).unwrap(),
            "\"installed_not_running\""
        );
        assert_eq!(
            serde_json::to_string(&RuntimeState::Healthy).unwrap(),
            "\"healthy\""
        );
    }

    #[test]
    fn plist_parser_reads_version_strings() {
        assert_eq!(
            plist_string_value(SAMPLE_PLIST, "CFBundleShortVersionString").as_deref(),
            Some("0.1.0")
        );
        assert_eq!(
            plist_string_value(SAMPLE_PLIST, "CFBundleIdentifier").as_deref(),
            Some("com.mars.driver")
        );
        assert_eq!(plist_string_value(SAMPLE_PLIST, "MissingKey"), None);
    }

    #[test]
    fn missing_components_win_over_everything() {
        assert_eq!(
            derive_state(false, PingOutcome::Responding, None, None, None),
            RuntimeState::Missing
        );
    }

    #[test]
    fn unreachable_daemon_is_installed_not_running() {
        assert_eq!(
            derive_state(
                true,
                PingOutcome::Unreachable,
                Some("0.1.0"),
                Some("0.1.0"),
                None
            ),
            RuntimeState::InstalledNotRunning
        );
    }

    #[test]
    fn protocol_mismatch_is_incompatible() {
        assert_eq!(
            derive_state(
                true,
                PingOutcome::ProtocolMismatch(Some(1)),
                None,
                None,
                None
            ),
            RuntimeState::Incompatible
        );
    }

    #[test]
    fn driver_daemon_major_mismatch_is_incompatible() {
        assert_eq!(
            derive_state(
                true,
                PingOutcome::Responding,
                Some("1.0.0"),
                Some("1.0.0"),
                Some("2.0.0")
            ),
            RuntimeState::Incompatible
        );
    }

    #[test]
    fn newer_files_than_running_daemon_is_stale() {
        assert_eq!(
            derive_state(
                true,
                PingOutcome::Responding,
                Some("0.2.0"),
                Some("0.2.0"),
                Some("0.1.0")
            ),
            RuntimeState::Stale
        );
    }

    #[test]
    fn matching_versions_are_healthy() {
        assert_eq!(
            derive_state(
                true,
                PingOutcome::Responding,
                Some("0.1.0"),
                Some("0.1.0"),
                Some("0.1.0")
            ),
            RuntimeState::Healthy
        );
    }

    #[test]
    fn unknown_versions_default_to_healthy_when_responding() {
        assert_eq!(
            derive_state(true, PingOutcome::Responding, None, None, None),
            RuntimeState::Healthy
        );
    }

    #[test]
    fn layout_for_home_uses_standard_paths() {
        let layout = RuntimeLayout::for_home(Path::new("/Users/example"));
        assert_eq!(layout.cli_path, Path::new("/usr/local/bin/mars"));
        assert_eq!(layout.daemon_path, Path::new("/usr/local/bin/marsd"));
        assert_eq!(
            layout.driver_bundle_path,
            Path::new("/Library/Audio/Plug-Ins/HAL/mars.driver")
        );
        assert_eq!(
            layout.launch_agent_path,
            Path::new("/Users/example/Library/LaunchAgents/com.mars.marsd.plist")
        );
        assert_eq!(
            layout.socket_path,
            Path::new("/Users/example/Library/Caches/mars/marsd.sock")
        );
    }
}

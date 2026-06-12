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
    /// Home directory of the user that owns the LaunchAgent.
    pub home: PathBuf,
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
            home: home.to_path_buf(),
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

// ---------------------------------------------------------------------------
// Package verification and install/update/uninstall
// ---------------------------------------------------------------------------

/// Manifest path of the `mars` binary inside a runtime package.
pub const PACKAGE_BIN_MARS: &str = "bin/mars";
/// Manifest path of the `marsd` binary inside a runtime package.
pub const PACKAGE_BIN_MARSD: &str = "bin/marsd";
/// Manifest path of the LaunchAgent template inside a runtime package.
pub const PACKAGE_LAUNCHD_PLIST: &str = "launchd/com.mars.marsd.plist";
/// Manifest directory of the driver bundle inside a runtime package.
pub const PACKAGE_DRIVER_BUNDLE: &str = "driver/mars.driver";

const CODESIGN_TIMEOUT: Duration = Duration::from_secs(30);
const STAPLER_TIMEOUT: Duration = Duration::from_secs(60);
const TAR_TIMEOUT: Duration = Duration::from_secs(120);
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Structured errors with stable machine-readable codes (see
/// [`RuntimeError::code`]) suitable for app UI.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("package manifest not found at {0}")]
    ManifestMissing(PathBuf),
    #[error("package manifest at {path} is invalid: {detail}")]
    ManifestInvalid { path: PathBuf, detail: String },
    #[error("package file path is not a safe relative path: {0}")]
    UnsafePath(String),
    #[error("package file missing: {0}")]
    FileMissing(PathBuf),
    #[error("sha256 mismatch for {path}: manifest={expected} actual={actual}")]
    DigestMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error(
        "package component '{path}' carries no codesign_id in the manifest; refusing to install an unsigned package (use --allow-unsigned for development builds only)"
    )]
    Unsigned { path: String },
    #[error("code signature verification failed for {path}: {detail}")]
    SignatureInvalid { path: PathBuf, detail: String },
    #[error("code signing authority mismatch for {path}: manifest='{expected}' actual='{actual}'")]
    SignerMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("notarization staple validation failed for {path}: {detail}")]
    StapleInvalid { path: PathBuf, detail: String },
    #[error("package speaks IPC protocol {package} but this SDK speaks {sdk}")]
    ProtocolUnsupported { package: u16, sdk: u16 },
    #[error("package requires macOS {required} or newer (host reports {actual})")]
    MacOsTooOld { required: String, actual: String },
    #[error("package version {package} is older than installed version {installed}")]
    VersionDowngrade { installed: String, package: String },
    #[error("no installed MARS runtime found; use `mars runtime install` first")]
    NotInstalled,
    #[error("command `{command}` failed: {detail}")]
    CommandFailed { command: String, detail: String },
    #[error("command `{command}` timed out after {timeout_ms} ms")]
    CommandTimedOut { command: String, timeout_ms: u64 },
    #[error("io error at {path}: {detail}")]
    Io { path: PathBuf, detail: String },
    #[error("cannot determine home directory")]
    HomeDirectoryUnavailable,
}

impl RuntimeError {
    /// Stable machine-readable error code for app UI.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ManifestMissing(_) => "manifest_missing",
            Self::ManifestInvalid { .. } => "manifest_invalid",
            Self::UnsafePath(_) => "unsafe_path",
            Self::FileMissing(_) => "file_missing",
            Self::DigestMismatch { .. } => "sha256_mismatch",
            Self::Unsigned { .. } => "unsigned_package",
            Self::SignatureInvalid { .. } => "signature_invalid",
            Self::SignerMismatch { .. } => "signer_mismatch",
            Self::StapleInvalid { .. } => "staple_invalid",
            Self::ProtocolUnsupported { .. } => "protocol_unsupported",
            Self::MacOsTooOld { .. } => "macos_too_old",
            Self::VersionDowngrade { .. } => "version_downgrade",
            Self::NotInstalled => "not_installed",
            Self::CommandFailed { .. } => "command_failed",
            Self::CommandTimedOut { .. } => "command_timed_out",
            Self::Io { .. } => "io_error",
            Self::HomeDirectoryUnavailable => "home_unavailable",
        }
    }
}

/// Options for [`verify_package`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Skip code-signature and notarization-staple checks. Exists for local
    /// development packages only; production installs must never set this.
    pub allow_unsigned: bool,
}

/// Verify an unpacked runtime package before any privileged copy:
/// manifest sha256 per file, `codesign --verify` against the manifest
/// `codesign_id`, `xcrun stapler validate` on the driver bundle, IPC protocol
/// compatibility, and the `min_macos` floor.
pub fn verify_package(
    package_dir: &Path,
    options: &VerifyOptions,
) -> Result<RuntimeManifest, RuntimeError> {
    let manifest_path = package_dir.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(RuntimeError::ManifestMissing(manifest_path));
    }
    let raw = std::fs::read_to_string(&manifest_path).map_err(|error| RuntimeError::Io {
        path: manifest_path.clone(),
        detail: error.to_string(),
    })?;
    let manifest: RuntimeManifest =
        serde_json::from_str(&raw).map_err(|error| RuntimeError::ManifestInvalid {
            path: manifest_path.clone(),
            detail: error.to_string(),
        })?;

    for required in [PACKAGE_BIN_MARS, PACKAGE_BIN_MARSD, PACKAGE_LAUNCHD_PLIST] {
        if !manifest.files.iter().any(|file| file.path == required) {
            return Err(RuntimeError::ManifestInvalid {
                path: manifest_path.clone(),
                detail: format!("missing required entry '{required}'"),
            });
        }
    }
    let bundle_prefix = format!("{PACKAGE_DRIVER_BUNDLE}/");
    if !manifest
        .files
        .iter()
        .any(|file| file.path.starts_with(&bundle_prefix))
    {
        return Err(RuntimeError::ManifestInvalid {
            path: manifest_path,
            detail: format!("missing driver bundle entries under '{PACKAGE_DRIVER_BUNDLE}/'"),
        });
    }

    if manifest.protocol_version != PROTOCOL_VERSION {
        return Err(RuntimeError::ProtocolUnsupported {
            package: manifest.protocol_version,
            sdk: PROTOCOL_VERSION,
        });
    }
    check_min_macos(&manifest.min_macos)?;

    for file in &manifest.files {
        validate_relative_path(&file.path)?;
        let full = package_dir.join(&file.path);
        if !full.is_file() {
            return Err(RuntimeError::FileMissing(full));
        }
        let actual = sha256_hex(&full)?;
        if !actual.eq_ignore_ascii_case(&file.sha256) {
            return Err(RuntimeError::DigestMismatch {
                path: full,
                expected: file.sha256.clone(),
                actual,
            });
        }
    }

    if !options.allow_unsigned {
        for target in signing_targets(&manifest) {
            let full = package_dir.join(&target.verify_path);
            let Some(expected) = target.codesign_id.as_deref() else {
                return Err(RuntimeError::Unsigned {
                    path: target.verify_path,
                });
            };
            verify_code_signature(&full, expected, target.is_bundle)?;
            if target.is_bundle {
                validate_staple(&full)?;
            }
        }
    }

    Ok(manifest)
}

#[derive(Debug, Clone)]
struct SigningTarget {
    /// Path (relative to the package root) handed to `codesign`.
    verify_path: String,
    codesign_id: Option<String>,
    is_bundle: bool,
}

fn signing_targets(manifest: &RuntimeManifest) -> Vec<SigningTarget> {
    let mut targets = Vec::new();
    for binary in [PACKAGE_BIN_MARS, PACKAGE_BIN_MARSD] {
        if let Some(entry) = manifest.files.iter().find(|file| file.path == binary) {
            targets.push(SigningTarget {
                verify_path: binary.to_string(),
                codesign_id: entry.codesign_id.clone(),
                is_bundle: false,
            });
        }
    }
    let bundle_prefix = format!("{PACKAGE_DRIVER_BUNDLE}/");
    if manifest
        .files
        .iter()
        .any(|file| file.path.starts_with(&bundle_prefix))
    {
        let codesign_id = manifest
            .files
            .iter()
            .filter(|file| file.path.starts_with(&bundle_prefix))
            .find_map(|file| file.codesign_id.clone());
        targets.push(SigningTarget {
            verify_path: PACKAGE_DRIVER_BUNDLE.to_string(),
            codesign_id,
            is_bundle: true,
        });
    }
    targets
}

fn verify_code_signature(
    target: &Path,
    expected_authority: &str,
    is_bundle: bool,
) -> Result<(), RuntimeError> {
    let target_str = target.display().to_string();
    let mut verify_args = vec!["--verify", "--strict"];
    if is_bundle {
        verify_args.push("--deep");
    }
    verify_args.push(&target_str);
    let verify = run_command("/usr/bin/codesign", &verify_args, CODESIGN_TIMEOUT)?;
    if verify.code != Some(0) {
        return Err(RuntimeError::SignatureInvalid {
            path: target.to_path_buf(),
            detail: combined_output(&verify),
        });
    }

    let display = run_command(
        "/usr/bin/codesign",
        &["-d", "--verbose=2", &target_str],
        CODESIGN_TIMEOUT,
    )?;
    let actual = parse_codesign_authority(&display.stderr)
        .or_else(|| parse_codesign_authority(&display.stdout));
    match actual {
        Some(actual) if actual == expected_authority => Ok(()),
        Some(actual) => Err(RuntimeError::SignerMismatch {
            path: target.to_path_buf(),
            expected: expected_authority.to_string(),
            actual,
        }),
        None => Err(RuntimeError::SignerMismatch {
            path: target.to_path_buf(),
            expected: expected_authority.to_string(),
            actual: "<no authority (ad-hoc or unsigned)>".to_string(),
        }),
    }
}

fn validate_staple(bundle: &Path) -> Result<(), RuntimeError> {
    let bundle_str = bundle.display().to_string();
    let result = run_command(
        "/usr/bin/xcrun",
        &["stapler", "validate", &bundle_str],
        STAPLER_TIMEOUT,
    )?;
    if result.code == Some(0) {
        Ok(())
    } else {
        Err(RuntimeError::StapleInvalid {
            path: bundle.to_path_buf(),
            detail: combined_output(&result),
        })
    }
}

fn parse_codesign_authority(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Authority=")
            .map(|value| value.trim().to_string())
    })
}

fn check_min_macos(required: &str) -> Result<(), RuntimeError> {
    let Ok(result) = run_command("/usr/bin/sw_vers", &["-productVersion"], PROBE_TIMEOUT) else {
        // Cannot determine the host version; do not block install on it.
        return Ok(());
    };
    if result.code != Some(0) {
        return Ok(());
    }
    let actual = result.stdout.trim().to_string();
    if actual.is_empty() {
        return Ok(());
    }
    if compare_versions(&actual, required) == std::cmp::Ordering::Less {
        return Err(RuntimeError::MacOsTooOld {
            required: required.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Compare dotted numeric versions segment-by-segment (missing segments
/// count as zero; non-numeric segments compare as zero).
#[must_use]
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |value: &str| -> Vec<u64> {
        value
            .split('.')
            .map(|segment| segment.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let left = parse(a);
    let right = parse(b);
    let len = left.len().max(right.len());
    for index in 0..len {
        let l = left.get(index).copied().unwrap_or(0);
        let r = right.get(index).copied().unwrap_or(0);
        match l.cmp(&r) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

fn validate_relative_path(path: &str) -> Result<(), RuntimeError> {
    if path.is_empty() {
        return Err(RuntimeError::UnsafePath(path.to_string()));
    }
    let parsed = Path::new(path);
    if parsed.is_absolute() {
        return Err(RuntimeError::UnsafePath(path.to_string()));
    }
    for component in parsed.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(RuntimeError::UnsafePath(path.to_string()));
        }
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String, RuntimeError> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|error| RuntimeError::Io {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|error| RuntimeError::Io {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Unpack a `mars-runtime-<version>.tar.gz` archive and return the package
/// root (the directory containing `manifest.json`).
pub fn unpack_package(archive: &Path, dest_dir: &Path) -> Result<PathBuf, RuntimeError> {
    if !archive.is_file() {
        return Err(RuntimeError::FileMissing(archive.to_path_buf()));
    }
    std::fs::create_dir_all(dest_dir).map_err(|error| RuntimeError::Io {
        path: dest_dir.to_path_buf(),
        detail: error.to_string(),
    })?;
    let archive_str = archive.display().to_string();
    let dest_str = dest_dir.display().to_string();
    let result = run_command(
        "/usr/bin/tar",
        &["-xzf", &archive_str, "-C", &dest_str],
        TAR_TIMEOUT,
    )?;
    if result.code != Some(0) {
        return Err(RuntimeError::CommandFailed {
            command: format!("tar -xzf {archive_str}"),
            detail: combined_output(&result),
        });
    }

    if dest_dir.join("manifest.json").is_file() {
        return Ok(dest_dir.to_path_buf());
    }
    // Tolerate archives that wrap everything in a single top-level directory.
    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(dest_dir).map_err(|error| RuntimeError::Io {
        path: dest_dir.to_path_buf(),
        detail: error.to_string(),
    })?;
    for entry in read_dir.flatten() {
        entries.push(entry.path());
    }
    if let [single] = entries.as_slice() {
        if single.is_dir() && single.join("manifest.json").is_file() {
            return Ok(single.clone());
        }
    }
    Err(RuntimeError::ManifestMissing(
        dest_dir.join("manifest.json"),
    ))
}

/// Render the single idempotent privileged install script. The host app runs
/// this through its own elevation flow (SMJobBless, osascript admin, sudo).
/// It only touches root-owned locations: `/usr/local/bin`,
/// `/Library/Audio/Plug-Ins/HAL`, and the coreaudiod reload.
#[must_use]
pub fn render_privileged_install_script(package_dir: &Path) -> String {
    let package = shell_quote(&package_dir.display().to_string());
    format!(
        r#"#!/bin/bash
# MARS privileged runtime install (generated by mars-sdk). Idempotent.
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "error: this script must run as root (sudo)." >&2
  exit 1
fi

PKG={package}
BIN_DIR="/usr/local/bin"
HAL_DIR="/Library/Audio/Plug-Ins/HAL"

install -d "$BIN_DIR"
install -m 0755 "$PKG/bin/mars" "$BIN_DIR/mars"
install -m 0755 "$PKG/bin/marsd" "$BIN_DIR/marsd"

install -d "$HAL_DIR"
rm -rf "$HAL_DIR/mars.driver"
cp -R "$PKG/driver/mars.driver" "$HAL_DIR/mars.driver"

# Reload coreaudiod so it picks up the new HAL driver.
if ! killall -9 coreaudiod 2>/dev/null; then
  launchctl kickstart -k system/com.apple.audio.coreaudiod || true
fi

echo "mars-runtime-privileged-install: ok"
"#
    )
}

/// Render the idempotent privileged uninstall script (reverse of
/// [`render_privileged_install_script`]).
#[must_use]
pub fn render_privileged_uninstall_script() -> String {
    r#"#!/bin/bash
# MARS privileged runtime uninstall (generated by mars-sdk). Idempotent.
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "error: this script must run as root (sudo)." >&2
  exit 1
fi

rm -f /usr/local/bin/mars /usr/local/bin/marsd
rm -rf /Library/Audio/Plug-Ins/HAL/mars.driver

# Reload coreaudiod so it drops the removed HAL driver.
if ! killall -9 coreaudiod 2>/dev/null; then
  launchctl kickstart -k system/com.apple.audio.coreaudiod || true
fi

echo "mars-runtime-privileged-uninstall: ok"
"#
    .to_string()
}

/// Write a script to disk with mode 0755.
pub fn write_executable_script(path: &Path, contents: &str) -> Result<(), RuntimeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| RuntimeError::Io {
            path: parent.to_path_buf(),
            detail: error.to_string(),
        })?;
    }
    std::fs::write(path, contents).map_err(|error| RuntimeError::Io {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(
            |error| RuntimeError::Io {
                path: path.to_path_buf(),
                detail: error.to_string(),
            },
        )?;
    }
    Ok(())
}

/// Result of the unprivileged (per-user) install steps.
#[derive(Debug, Clone, Serialize)]
pub struct UserInstallReport {
    pub launch_agent_path: String,
    pub receipt_path: String,
    pub bootstrapped: bool,
    pub kickstarted: bool,
    pub notes: Vec<String>,
}

/// Result of the unprivileged (per-user) uninstall steps.
#[derive(Debug, Clone, Serialize)]
pub struct UserUninstallReport {
    pub removed_launch_agent: bool,
    pub removed_receipt: bool,
    pub notes: Vec<String>,
}

/// Perform the per-user install steps (no elevation required): support
/// directories, LaunchAgent render + bootstrap, and the install receipt.
/// Mirrors what `scripts/install.sh` does today, minus building from source.
pub fn install_user_components(
    layout: &RuntimeLayout,
    package_dir: &Path,
    manifest: &RuntimeManifest,
) -> Result<UserInstallReport, RuntimeError> {
    let mut notes = Vec::new();

    for relative in [
        "Library/Logs/mars",
        "Library/Caches/mars",
        "Library/Application Support/mars/profiles",
    ] {
        let dir = layout.home.join(relative);
        std::fs::create_dir_all(&dir).map_err(|error| RuntimeError::Io {
            path: dir.clone(),
            detail: error.to_string(),
        })?;
    }

    let template_path = package_dir.join(PACKAGE_LAUNCHD_PLIST);
    let template = std::fs::read_to_string(&template_path).map_err(|error| RuntimeError::Io {
        path: template_path.clone(),
        detail: error.to_string(),
    })?;
    let bin_dir = layout
        .daemon_path
        .parent()
        .unwrap_or_else(|| Path::new(DEFAULT_BIN_DIR));
    let rendered = template
        .replace("__MARS_BIN__", &bin_dir.display().to_string())
        .replace("__HOME__", &layout.home.display().to_string());

    if let Some(parent) = layout.launch_agent_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| RuntimeError::Io {
            path: parent.to_path_buf(),
            detail: error.to_string(),
        })?;
    }
    match std::fs::remove_file(&layout.launch_agent_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RuntimeError::Io {
                path: layout.launch_agent_path.clone(),
                detail: format!("cannot replace existing LaunchAgent plist: {error}"),
            });
        }
    }
    std::fs::write(&layout.launch_agent_path, rendered).map_err(|error| RuntimeError::Io {
        path: layout.launch_agent_path.clone(),
        detail: error.to_string(),
    })?;

    let uid = current_uid()?;
    let domain_target = format!("gui/{uid}");
    let service_target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
    let plist_str = layout.launch_agent_path.display().to_string();

    // Best-effort: the service may not be bootstrapped yet.
    let _ = launchctl(&["bootout", &service_target]);

    let bootstrap = launchctl(&["bootstrap", &domain_target, &plist_str])?;
    let bootstrapped = bootstrap.code == Some(0);
    if !bootstrapped {
        notes.push(format!(
            "launchctl bootstrap failed: {}",
            combined_output(&bootstrap)
        ));
    }
    let enable = launchctl(&["enable", &service_target])?;
    if enable.code != Some(0) {
        notes.push(format!(
            "launchctl enable failed: {}",
            combined_output(&enable)
        ));
    }
    let kickstart = launchctl(&["kickstart", "-k", &service_target])?;
    let kickstarted = kickstart.code == Some(0);
    if !kickstarted {
        notes.push(format!(
            "launchctl kickstart failed: {}",
            combined_output(&kickstart)
        ));
    }
    if !layout.daemon_path.is_file() {
        notes.push(format!(
            "{} is not installed yet; launchd (KeepAlive) starts the daemon automatically once the privileged install script has run",
            layout.daemon_path.display()
        ));
    }

    if let Some(parent) = layout.receipt_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| RuntimeError::Io {
            path: parent.to_path_buf(),
            detail: error.to_string(),
        })?;
    }
    let receipt =
        serde_json::to_string_pretty(manifest).map_err(|error| RuntimeError::ManifestInvalid {
            path: layout.receipt_path.clone(),
            detail: error.to_string(),
        })?;
    std::fs::write(&layout.receipt_path, receipt).map_err(|error| RuntimeError::Io {
        path: layout.receipt_path.clone(),
        detail: error.to_string(),
    })?;

    Ok(UserInstallReport {
        launch_agent_path: layout.launch_agent_path.display().to_string(),
        receipt_path: layout.receipt_path.display().to_string(),
        bootstrapped,
        kickstarted,
        notes,
    })
}

/// Stop the daemon by booting the LaunchAgent out of the user's gui domain.
/// Best effort: returns `Ok(false)` when nothing was bootstrapped.
pub fn bootout_daemon() -> Result<bool, RuntimeError> {
    let uid = current_uid()?;
    let service_target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
    match launchctl(&["bootout", &service_target]) {
        Ok(result) => Ok(result.code == Some(0)),
        Err(_) => Ok(false),
    }
}

/// Perform the per-user uninstall steps (no elevation required). Idempotent.
pub fn uninstall_user_components(
    layout: &RuntimeLayout,
) -> Result<UserUninstallReport, RuntimeError> {
    let mut notes = Vec::new();

    if let Ok(uid) = current_uid() {
        let service_target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
        let _ = launchctl(&["bootout", &service_target]);
    } else {
        notes.push("could not determine uid; skipped launchctl bootout".to_string());
    }

    let removed_launch_agent = remove_file_if_present(&layout.launch_agent_path, &mut notes);
    let removed_receipt = remove_file_if_present(&layout.receipt_path, &mut notes);

    let caches = layout.home.join("Library/Caches/mars");
    match std::fs::remove_dir_all(&caches) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => notes.push(format!("failed to remove {}: {error}", caches.display())),
    }

    notes.push(
        "orphaned POSIX shared-memory rings named 'mars.*' cannot be enumerated from user space; they are released when their owning processes exit or at reboot"
            .to_string(),
    );

    Ok(UserUninstallReport {
        removed_launch_agent,
        removed_receipt,
        notes,
    })
}

fn remove_file_if_present(path: &Path, notes: &mut Vec<String>) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            notes.push(format!("failed to remove {}: {error}", path.display()));
            false
        }
    }
}

/// Numeric uid of the current user (used for `gui/<uid>` launchd targets).
pub fn current_uid() -> Result<u32, RuntimeError> {
    let result = run_command("/usr/bin/id", &["-u"], PROBE_TIMEOUT)?;
    if result.code != Some(0) {
        return Err(RuntimeError::CommandFailed {
            command: "id -u".to_string(),
            detail: combined_output(&result),
        });
    }
    result
        .stdout
        .trim()
        .parse::<u32>()
        .map_err(|error| RuntimeError::CommandFailed {
            command: "id -u".to_string(),
            detail: format!("unparseable uid '{}': {error}", result.stdout.trim()),
        })
}

fn launchctl(args: &[&str]) -> Result<CommandOutput, RuntimeError> {
    run_command("/bin/launchctl", args, LAUNCHCTL_TIMEOUT)
}

#[derive(Debug, Clone)]
struct CommandOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn combined_output(output: &CommandOutput) -> String {
    let mut parts = Vec::new();
    if let Some(code) = output.code {
        parts.push(format!("exit={code}"));
    }
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        parts.push(stdout.to_string());
    }
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        parts.push(stderr.to_string());
    }
    if parts.is_empty() {
        "no output".to_string()
    } else {
        parts.join("; ")
    }
}

/// Run a command with an explicit deadline so installer flows can never hang.
fn run_command(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<CommandOutput, RuntimeError> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let command_display = format!("{program} {}", args.join(" "));
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| RuntimeError::CommandFailed {
            command: command_display.clone(),
            detail: error.to_string(),
        })?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_string(&mut stdout);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Ok(CommandOutput {
                    code: status.code(),
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RuntimeError::CommandTimedOut {
                        command: command_display,
                        timeout_ms: timeout.as_millis() as u64,
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(RuntimeError::CommandFailed {
                    command: command_display,
                    detail: error.to_string(),
                });
            }
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("mars-sdk-runtime-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_package_file(root: &Path, relative: &str, contents: &str) -> ManifestFile {
        let full = root.join(relative);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, contents).unwrap();
        ManifestFile {
            path: relative.to_string(),
            sha256: sha256_hex(&full).unwrap(),
            codesign_id: None,
        }
    }

    fn sample_package(root: &Path) -> RuntimeManifest {
        let files = vec![
            write_package_file(root, PACKAGE_BIN_MARS, "mars-cli"),
            write_package_file(root, PACKAGE_BIN_MARSD, "mars-daemon"),
            write_package_file(root, PACKAGE_LAUNCHD_PLIST, "<plist/>"),
            write_package_file(
                root,
                "driver/mars.driver/Contents/Info.plist",
                "<plist><dict/></plist>",
            ),
            write_package_file(root, "driver/mars.driver/Contents/MacOS/mars_hal", "hal"),
        ];
        let manifest = RuntimeManifest {
            version: "0.1.0".to_string(),
            min_macos: "15.0".to_string(),
            protocol_version: PROTOCOL_VERSION,
            files,
        };
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        manifest
    }

    #[test]
    fn verify_package_accepts_valid_unsigned_package_with_allow_unsigned() {
        let temp = TempDir::new("verify-ok");
        let manifest = sample_package(temp.path());

        let verified = verify_package(
            temp.path(),
            &VerifyOptions {
                allow_unsigned: true,
            },
        )
        .unwrap();
        assert_eq!(verified, manifest);
    }

    #[test]
    fn verify_package_rejects_unsigned_package_by_default() {
        let temp = TempDir::new("verify-unsigned");
        let _ = sample_package(temp.path());

        let error = verify_package(temp.path(), &VerifyOptions::default()).unwrap_err();
        assert_eq!(error.code(), "unsigned_package");
    }

    #[test]
    fn verify_package_detects_tampered_files() {
        let temp = TempDir::new("verify-tamper");
        let _ = sample_package(temp.path());
        std::fs::write(temp.path().join(PACKAGE_BIN_MARSD), "tampered").unwrap();

        let error = verify_package(
            temp.path(),
            &VerifyOptions {
                allow_unsigned: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "sha256_mismatch");
    }

    #[test]
    fn verify_package_requires_manifest() {
        let temp = TempDir::new("verify-no-manifest");
        let error = verify_package(temp.path(), &VerifyOptions::default()).unwrap_err();
        assert_eq!(error.code(), "manifest_missing");
    }

    #[test]
    fn verify_package_rejects_protocol_mismatch() {
        let temp = TempDir::new("verify-protocol");
        let mut manifest = sample_package(temp.path());
        manifest.protocol_version = PROTOCOL_VERSION + 1;
        std::fs::write(
            temp.path().join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = verify_package(
            temp.path(),
            &VerifyOptions {
                allow_unsigned: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "protocol_unsupported");
    }

    #[test]
    fn verify_package_rejects_unsafe_paths() {
        let temp = TempDir::new("verify-unsafe");
        let mut manifest = sample_package(temp.path());
        manifest.files.push(ManifestFile {
            path: "../escape".to_string(),
            sha256: "00".repeat(32),
            codesign_id: None,
        });
        std::fs::write(
            temp.path().join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = verify_package(
            temp.path(),
            &VerifyOptions {
                allow_unsigned: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "unsafe_path");
    }

    #[test]
    fn verify_package_requires_core_entries() {
        let temp = TempDir::new("verify-core");
        let mut manifest = sample_package(temp.path());
        manifest.files.retain(|file| file.path != PACKAGE_BIN_MARSD);
        std::fs::write(
            temp.path().join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let error = verify_package(
            temp.path(),
            &VerifyOptions {
                allow_unsigned: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "manifest_invalid");
    }

    #[test]
    fn unpack_package_finds_root_level_and_nested_manifests() {
        let temp = TempDir::new("unpack");
        let stage = temp.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let _ = sample_package(&stage);

        // Root-level layout (manifest.json at archive root).
        let flat_archive = temp.path().join("flat.tar.gz");
        let stage_str = stage.display().to_string();
        let flat_str = flat_archive.display().to_string();
        let result = run_command(
            "/usr/bin/tar",
            &["-czf", &flat_str, "-C", &stage_str, "."],
            TAR_TIMEOUT,
        )
        .unwrap();
        assert_eq!(result.code, Some(0));
        let flat_dest = temp.path().join("flat-out");
        let root = unpack_package(&flat_archive, &flat_dest).unwrap();
        assert_eq!(root, flat_dest);
        assert!(root.join("manifest.json").is_file());

        // Single wrapping directory layout.
        let parent = stage.parent().unwrap().display().to_string();
        let nested_archive = temp.path().join("nested.tar.gz");
        let nested_str = nested_archive.display().to_string();
        let result = run_command(
            "/usr/bin/tar",
            &["-czf", &nested_str, "-C", &parent, "stage"],
            TAR_TIMEOUT,
        )
        .unwrap();
        assert_eq!(result.code, Some(0));
        let nested_dest = temp.path().join("nested-out");
        let root = unpack_package(&nested_archive, &nested_dest).unwrap();
        assert_eq!(root, nested_dest.join("stage"));
        assert!(root.join("manifest.json").is_file());
    }

    #[test]
    fn unpack_package_requires_archive() {
        let temp = TempDir::new("unpack-missing");
        let error =
            unpack_package(&temp.path().join("nope.tar.gz"), &temp.path().join("out")).unwrap_err();
        assert_eq!(error.code(), "file_missing");
    }

    #[test]
    fn compare_versions_orders_numeric_segments() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("0.1.0", "0.1.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.2.0", "0.1.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.1", "0.1.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.1.1", "0.1"), Ordering::Greater);
        assert_eq!(compare_versions("9.9.9", "10.0"), Ordering::Less);
        assert_eq!(compare_versions("15.5", "15.0"), Ordering::Greater);
    }

    #[test]
    fn privileged_install_script_is_idempotent_and_quoted() {
        let script = render_privileged_install_script(Path::new("/tmp/mars pkg/it's-here"));
        assert!(script.starts_with("#!/bin/bash"));
        assert!(script.contains("set -euo pipefail"));
        assert!(script.contains(r#"PKG='/tmp/mars pkg/it'\''s-here'"#));
        assert!(script.contains(r#"rm -rf "$HAL_DIR/mars.driver""#));
        assert!(script.contains(r#"install -m 0755 "$PKG/bin/marsd" "$BIN_DIR/marsd""#));
        assert!(script.contains("killall -9 coreaudiod"));
        // Privileged script must never touch per-user state.
        assert!(!script.contains("LaunchAgents"));
        assert!(!script.contains("bootstrap"));
    }

    #[test]
    fn privileged_uninstall_script_removes_all_root_owned_artifacts() {
        let script = render_privileged_uninstall_script();
        assert!(script.contains("rm -f /usr/local/bin/mars /usr/local/bin/marsd"));
        assert!(script.contains("rm -rf /Library/Audio/Plug-Ins/HAL/mars.driver"));
        assert!(script.contains("killall -9 coreaudiod"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
    }

    #[test]
    fn parse_codesign_authority_reads_first_authority_line() {
        let output = "Identifier=marsd\nAuthority=Developer ID Application: Example (TEAM)\nAuthority=Developer ID Certification Authority\n";
        assert_eq!(
            parse_codesign_authority(output).as_deref(),
            Some("Developer ID Application: Example (TEAM)")
        );
        assert_eq!(parse_codesign_authority("Signature=adhoc\n"), None);
    }

    #[test]
    fn run_command_enforces_deadline() {
        let error = run_command("/bin/sleep", &["5"], Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.code(), "command_timed_out");
    }

    #[test]
    fn validate_relative_path_rejects_traversal_and_absolute() {
        assert!(validate_relative_path("bin/mars").is_ok());
        assert!(validate_relative_path("../evil").is_err());
        assert!(validate_relative_path("/etc/passwd").is_err());
        assert!(validate_relative_path("a/../b").is_err());
        assert!(validate_relative_path("").is_err());
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

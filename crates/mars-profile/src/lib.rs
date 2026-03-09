#![forbid(unsafe_code)]
//! YAML profile parsing, schema generation, and semantic validation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use mars_graph::{GraphError, RoutingGraph, build_routing_graph};
use mars_types::{AutoOrU16, AutoOrU32, PROFILE_VERSION, ProcessorKind, Profile, ValidationReport};
use regex::Regex;
use schemars::schema_for;
use thiserror::Error;

#[derive(Debug)]
pub struct ValidatedProfile {
    pub profile: Profile,
    pub graph: RoutingGraph,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum TemplateKind {
    Default,
    Multi,
    Blank,
}

impl TemplateKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "multi" => Some(Self::Multi),
            "blank" => Some(Self::Blank),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("failed to read profile at {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid yaml: {0}")]
    Yaml(serde_yaml::Error),
    #[error("unsupported profile version: {0}")]
    UnsupportedVersion(u32),
    #[error("invalid id '{id}': must match [a-zA-Z0-9][a-zA-Z0-9-_]{{0,63}}")]
    InvalidId { id: String },
    #[error("duplicate id '{id}' in profile")]
    DuplicateId { id: String },
    #[error("audio.sample_rate must be positive integer or 'auto'")]
    InvalidSampleRate,
    #[error("audio.channels must be positive integer or 'auto'")]
    InvalidChannels,
    #[error("audio.buffer_frames must be > 0")]
    InvalidBufferFrames,
    #[error("pan must be in [-1.0, 1.0], got {0}")]
    InvalidPan(f32),
    #[error("route '{route_id}' pan must be in [-1.0, 1.0], got {value}")]
    InvalidRoutePan { route_id: String, value: f32 },
    #[error("pipe delay must be in [0.0, 2000.0], got {0}")]
    InvalidDelay(f32),
    #[error("route '{route_id}' delay must be in [0.0, 2000.0], got {value}")]
    InvalidRouteDelay { route_id: String, value: f32 },
    #[error("route '{route_id}' references unknown source '{source_id}'")]
    UnknownRouteSource { route_id: String, source_id: String },
    #[error("route '{route_id}' references unknown destination '{destination}'")]
    UnknownRouteDestination {
        route_id: String,
        destination: String,
    },
    #[error("route '{route_id}' references unknown processor chain '{chain_id}'")]
    UnknownRouteChain { route_id: String, chain_id: String },
    #[error(
        "route '{route_id}' matrix shape mismatch: declared rows={rows}, cols={cols}, actual_rows={actual_rows}, actual_cols={actual_cols}"
    )]
    RouteMatrixShapeMismatch {
        route_id: String,
        rows: u16,
        cols: u16,
        actual_rows: usize,
        actual_cols: usize,
    },
    #[error(
        "route '{route_id}' matrix channels mismatch: expected rows={expected_rows}, cols={expected_cols}, got rows={rows}, cols={cols}"
    )]
    RouteMatrixChannelMismatch {
        route_id: String,
        expected_rows: u16,
        expected_cols: u16,
        rows: u16,
        cols: u16,
    },
    #[error("route '{route_id}' matrix must be finite at [{row}][{col}], got {value}")]
    NonFiniteRouteMatrixCoefficient {
        route_id: String,
        row: usize,
        col: usize,
        value: f32,
    },
    #[error("processor chain '{chain_id}' references unknown processor '{processor_id}'")]
    UnknownProcessorInChain {
        chain_id: String,
        processor_id: String,
    },
    #[error("processor '{processor_id}' eq config is invalid: {reason}")]
    InvalidEqConfig {
        processor_id: String,
        reason: String,
    },
    #[error("processor '{processor_id}' dynamics config is invalid: {reason}")]
    InvalidDynamicsConfig {
        processor_id: String,
        reason: String,
    },
    #[error("processor '{processor_id}' denoise config is invalid: {reason}")]
    InvalidDenoiseConfig {
        processor_id: String,
        reason: String,
    },
    #[error("processor '{processor_id}' time-shift config is invalid: {reason}")]
    InvalidTimeShiftConfig {
        processor_id: String,
        reason: String,
    },
    #[error("file sink '{sink_id}' path must not be empty")]
    EmptyFileSinkPath { sink_id: String },
    #[error("stream sink '{sink_id}' endpoint must not be empty")]
    EmptyStreamSinkEndpoint { sink_id: String },
    #[error("invalid name_regex for external endpoint '{id}': '{pattern}' ({reason})")]
    InvalidNameRegex {
        id: String,
        pattern: String,
        reason: String,
    },
    #[error("graph validation failed: {0}")]
    Graph(#[from] GraphError),
}

pub fn load_profile(path: &Path) -> Result<ValidatedProfile, ProfileError> {
    let raw = fs::read_to_string(path).map_err(|source| ProfileError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let profile = parse_profile_str(&raw)?;
    validate_profile(profile)
}

pub fn parse_profile_str(raw: &str) -> Result<Profile, ProfileError> {
    let profile: Profile = serde_yaml::from_str(raw).map_err(ProfileError::Yaml)?;
    if profile.version != PROFILE_VERSION {
        return Err(ProfileError::UnsupportedVersion(profile.version));
    }
    Ok(profile)
}

pub fn validate_profile(profile: Profile) -> Result<ValidatedProfile, ProfileError> {
    if profile.version != PROFILE_VERSION {
        return Err(ProfileError::UnsupportedVersion(profile.version));
    }

    validate_audio(&profile)?;
    validate_ids(&profile)?;
    validate_pipe_ranges(&profile)?;
    validate_routes(&profile)?;
    validate_processor_chains(&profile)?;
    validate_sinks(&profile)?;
    validate_external_matchers(&profile)?;
    let graph = build_routing_graph(&profile)?;

    let warnings = Vec::new();
    Ok(ValidatedProfile {
        profile,
        graph,
        warnings,
    })
}

pub fn validate_only(path: &Path) -> ValidationReport {
    match load_profile(path) {
        Ok(validated) => ValidationReport {
            valid: true,
            warnings: validated.warnings,
            errors: Vec::new(),
        },
        Err(error) => ValidationReport {
            valid: false,
            warnings: Vec::new(),
            errors: vec![error.to_string()],
        },
    }
}

pub fn profile_schema_json() -> serde_json::Value {
    match serde_json::to_value(schema_for!(Profile)) {
        Ok(value) => value,
        Err(error) => serde_json::json!({
            "error": format!("failed to serialize profile schema: {error}")
        }),
    }
}

pub fn render_template(name: &str, template: TemplateKind) -> String {
    match template {
        TemplateKind::Default => default_template(name),
        TemplateKind::Multi => multi_template(name),
        TemplateKind::Blank => blank_template(name),
    }
}

fn validate_audio(profile: &Profile) -> Result<(), ProfileError> {
    match &profile.audio.sample_rate {
        AutoOrU32::Value(value) if *value > 0 => {}
        AutoOrU32::Auto(value) if value == "auto" => {}
        _ => return Err(ProfileError::InvalidSampleRate),
    }

    match &profile.audio.channels {
        AutoOrU16::Value(value) if *value > 0 => {}
        AutoOrU16::Auto(value) if value == "auto" => {}
        _ => return Err(ProfileError::InvalidChannels),
    }

    if profile.audio.buffer_frames == 0 {
        return Err(ProfileError::InvalidBufferFrames);
    }

    Ok(())
}

fn validate_pipe_ranges(profile: &Profile) -> Result<(), ProfileError> {
    for pipe in &profile.pipes {
        if !(-1.0..=1.0).contains(&pipe.pan) {
            return Err(ProfileError::InvalidPan(pipe.pan));
        }
        if !(0.0..=2_000.0).contains(&pipe.delay_ms) {
            return Err(ProfileError::InvalidDelay(pipe.delay_ms));
        }
    }
    Ok(())
}

fn validate_routes(profile: &Profile) -> Result<(), ProfileError> {
    let node_channels = collect_node_channels(profile);
    let chain_ids = profile
        .processor_chains
        .iter()
        .map(|chain| chain.id.as_str())
        .collect::<BTreeSet<_>>();

    for route in &profile.routes {
        if !(-1.0..=1.0).contains(&route.pan) {
            return Err(ProfileError::InvalidRoutePan {
                route_id: route.id.clone(),
                value: route.pan,
            });
        }
        if !(0.0..=2_000.0).contains(&route.delay_ms) {
            return Err(ProfileError::InvalidRouteDelay {
                route_id: route.id.clone(),
                value: route.delay_ms,
            });
        }

        let source_channels = node_channels
            .get(route.from.as_str())
            .copied()
            .ok_or_else(|| ProfileError::UnknownRouteSource {
                route_id: route.id.clone(),
                source_id: route.from.clone(),
            })?;
        let destination_channels =
            node_channels
                .get(route.to.as_str())
                .copied()
                .ok_or_else(|| ProfileError::UnknownRouteDestination {
                    route_id: route.id.clone(),
                    destination: route.to.clone(),
                })?;

        if let Some(chain_id) = route.chain.as_deref() {
            if !chain_ids.contains(chain_id) {
                return Err(ProfileError::UnknownRouteChain {
                    route_id: route.id.clone(),
                    chain_id: chain_id.to_string(),
                });
            }
        }

        let rows = route.matrix.rows as usize;
        let cols = route.matrix.cols as usize;
        if rows == 0 || cols == 0 {
            return Err(ProfileError::RouteMatrixShapeMismatch {
                route_id: route.id.clone(),
                rows: route.matrix.rows,
                cols: route.matrix.cols,
                actual_rows: route.matrix.coefficients.len(),
                actual_cols: route.matrix.coefficients.first().map_or(0, Vec::len),
            });
        }

        if rows != route.matrix.coefficients.len()
            || route
                .matrix
                .coefficients
                .iter()
                .any(|row| row.len() != cols)
        {
            return Err(ProfileError::RouteMatrixShapeMismatch {
                route_id: route.id.clone(),
                rows: route.matrix.rows,
                cols: route.matrix.cols,
                actual_rows: route.matrix.coefficients.len(),
                actual_cols: route.matrix.coefficients.first().map_or(0, Vec::len),
            });
        }

        if route.matrix.rows != destination_channels || route.matrix.cols != source_channels {
            return Err(ProfileError::RouteMatrixChannelMismatch {
                route_id: route.id.clone(),
                expected_rows: destination_channels,
                expected_cols: source_channels,
                rows: route.matrix.rows,
                cols: route.matrix.cols,
            });
        }

        for (row_index, row) in route.matrix.coefficients.iter().enumerate() {
            for (col_index, coefficient) in row.iter().enumerate() {
                if !coefficient.is_finite() {
                    return Err(ProfileError::NonFiniteRouteMatrixCoefficient {
                        route_id: route.id.clone(),
                        row: row_index,
                        col: col_index,
                        value: *coefficient,
                    });
                }
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EqConfig {
    #[serde(default)]
    bands: Vec<EqBandConfig>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EqBandConfig {
    #[serde(default = "default_eq_freq_hz")]
    freq_hz: f32,
    #[serde(default = "default_eq_q")]
    q: f32,
    #[serde(default)]
    gain_db: f32,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DynamicsConfig {
    #[serde(default = "default_dynamics_threshold_db")]
    threshold_db: f32,
    #[serde(default = "default_dynamics_ratio")]
    ratio: f32,
    #[serde(default = "default_dynamics_attack_ms")]
    attack_ms: f32,
    #[serde(default = "default_dynamics_release_ms")]
    release_ms: f32,
    #[serde(default)]
    makeup_gain_db: f32,
    #[serde(default)]
    limiter: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DenoiseConfig {
    #[serde(default = "default_denoise_threshold_db")]
    threshold_db: f32,
    #[serde(default = "default_denoise_reduction_db")]
    reduction_db: f32,
    #[serde(default = "default_denoise_attack_ms")]
    attack_ms: f32,
    #[serde(default = "default_denoise_release_ms")]
    release_ms: f32,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TimeShiftConfig {
    #[serde(default)]
    delay_ms: f32,
    #[serde(default = "default_timeshift_max_delay_ms")]
    max_delay_ms: f32,
}

const fn default_true() -> bool {
    true
}

const fn default_eq_freq_hz() -> f32 {
    1_000.0
}

const fn default_eq_q() -> f32 {
    1.0
}

const fn default_dynamics_threshold_db() -> f32 {
    -18.0
}

const fn default_dynamics_ratio() -> f32 {
    4.0
}

const fn default_dynamics_attack_ms() -> f32 {
    10.0
}

const fn default_dynamics_release_ms() -> f32 {
    100.0
}

const fn default_denoise_threshold_db() -> f32 {
    -45.0
}

const fn default_denoise_reduction_db() -> f32 {
    18.0
}

const fn default_denoise_attack_ms() -> f32 {
    5.0
}

const fn default_denoise_release_ms() -> f32 {
    120.0
}

const fn default_timeshift_max_delay_ms() -> f32 {
    2_000.0
}

fn validate_eq_config(config: &EqConfig, processor_id: &str) -> Result<(), ProfileError> {
    if config.bands.len() > 16 {
        return Err(ProfileError::InvalidEqConfig {
            processor_id: processor_id.to_string(),
            reason: "bands length must be <= 16".to_string(),
        });
    }

    for (index, band) in config.bands.iter().enumerate() {
        if !band.freq_hz.is_finite() || band.freq_hz <= 0.0 || band.freq_hz > 24_000.0 {
            return Err(ProfileError::InvalidEqConfig {
                processor_id: processor_id.to_string(),
                reason: format!("band[{index}] freq_hz must be finite in (0, 24000]"),
            });
        }
        if !band.q.is_finite() || band.q <= 0.0 || band.q > 24.0 {
            return Err(ProfileError::InvalidEqConfig {
                processor_id: processor_id.to_string(),
                reason: format!("band[{index}] q must be finite in (0, 24]"),
            });
        }
        if !band.gain_db.is_finite() || band.gain_db < -36.0 || band.gain_db > 36.0 {
            return Err(ProfileError::InvalidEqConfig {
                processor_id: processor_id.to_string(),
                reason: format!("band[{index}] gain_db must be finite in [-36, 36]"),
            });
        }
        let _ = band.enabled;
    }
    Ok(())
}

fn validate_dynamics_config(
    config: &DynamicsConfig,
    processor_id: &str,
) -> Result<(), ProfileError> {
    let in_range =
        |value: f32, min: f32, max: f32| value.is_finite() && value >= min && value <= max;
    if !in_range(config.threshold_db, -96.0, 0.0) {
        return Err(ProfileError::InvalidDynamicsConfig {
            processor_id: processor_id.to_string(),
            reason: "threshold_db must be finite in [-96, 0]".to_string(),
        });
    }
    if !in_range(config.ratio, 1.0, 32.0) {
        return Err(ProfileError::InvalidDynamicsConfig {
            processor_id: processor_id.to_string(),
            reason: "ratio must be finite in [1, 32]".to_string(),
        });
    }
    if !in_range(config.attack_ms, 0.1, 500.0) {
        return Err(ProfileError::InvalidDynamicsConfig {
            processor_id: processor_id.to_string(),
            reason: "attack_ms must be finite in [0.1, 500]".to_string(),
        });
    }
    if !in_range(config.release_ms, 1.0, 5_000.0) {
        return Err(ProfileError::InvalidDynamicsConfig {
            processor_id: processor_id.to_string(),
            reason: "release_ms must be finite in [1, 5000]".to_string(),
        });
    }
    if !in_range(config.makeup_gain_db, -36.0, 36.0) {
        return Err(ProfileError::InvalidDynamicsConfig {
            processor_id: processor_id.to_string(),
            reason: "makeup_gain_db must be finite in [-36, 36]".to_string(),
        });
    }
    let _ = config.limiter;
    Ok(())
}

fn validate_denoise_config(config: &DenoiseConfig, processor_id: &str) -> Result<(), ProfileError> {
    let in_range =
        |value: f32, min: f32, max: f32| value.is_finite() && value >= min && value <= max;
    if !in_range(config.threshold_db, -96.0, 0.0) {
        return Err(ProfileError::InvalidDenoiseConfig {
            processor_id: processor_id.to_string(),
            reason: "threshold_db must be finite in [-96, 0]".to_string(),
        });
    }
    if !in_range(config.reduction_db, 0.0, 60.0) {
        return Err(ProfileError::InvalidDenoiseConfig {
            processor_id: processor_id.to_string(),
            reason: "reduction_db must be finite in [0, 60]".to_string(),
        });
    }
    if !in_range(config.attack_ms, 0.1, 500.0) {
        return Err(ProfileError::InvalidDenoiseConfig {
            processor_id: processor_id.to_string(),
            reason: "attack_ms must be finite in [0.1, 500]".to_string(),
        });
    }
    if !in_range(config.release_ms, 1.0, 5_000.0) {
        return Err(ProfileError::InvalidDenoiseConfig {
            processor_id: processor_id.to_string(),
            reason: "release_ms must be finite in [1, 5000]".to_string(),
        });
    }
    Ok(())
}

fn validate_time_shift_config(
    config: &TimeShiftConfig,
    processor_id: &str,
) -> Result<(), ProfileError> {
    let in_range =
        |value: f32, min: f32, max: f32| value.is_finite() && value >= min && value <= max;
    if !in_range(config.delay_ms, 0.0, 2_000.0) {
        return Err(ProfileError::InvalidTimeShiftConfig {
            processor_id: processor_id.to_string(),
            reason: "delay_ms must be finite in [0, 2000]".to_string(),
        });
    }
    if !in_range(config.max_delay_ms, 1.0, 2_000.0) {
        return Err(ProfileError::InvalidTimeShiftConfig {
            processor_id: processor_id.to_string(),
            reason: "max_delay_ms must be finite in [1, 2000]".to_string(),
        });
    }
    if config.delay_ms > config.max_delay_ms {
        return Err(ProfileError::InvalidTimeShiftConfig {
            processor_id: processor_id.to_string(),
            reason: "delay_ms must be <= max_delay_ms".to_string(),
        });
    }
    Ok(())
}

fn normalized_processor_config(config: &serde_json::Value) -> serde_json::Value {
    if config.is_null() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    config.clone()
}

fn validate_processor_chains(profile: &Profile) -> Result<(), ProfileError> {
    for processor in &profile.processors {
        match processor.kind {
            ProcessorKind::Eq => {
                let config = serde_json::from_value::<EqConfig>(normalized_processor_config(
                    &processor.config,
                ))
                .map_err(|error| ProfileError::InvalidEqConfig {
                    processor_id: processor.id.clone(),
                    reason: error.to_string(),
                })?;
                validate_eq_config(&config, &processor.id)?;
            }
            ProcessorKind::Dynamics => {
                let config = serde_json::from_value::<DynamicsConfig>(normalized_processor_config(
                    &processor.config,
                ))
                .map_err(|error| ProfileError::InvalidDynamicsConfig {
                    processor_id: processor.id.clone(),
                    reason: error.to_string(),
                })?;
                validate_dynamics_config(&config, &processor.id)?;
            }
            ProcessorKind::Denoise => {
                let config = serde_json::from_value::<DenoiseConfig>(normalized_processor_config(
                    &processor.config,
                ))
                .map_err(|error| ProfileError::InvalidDenoiseConfig {
                    processor_id: processor.id.clone(),
                    reason: error.to_string(),
                })?;
                validate_denoise_config(&config, &processor.id)?;
            }
            ProcessorKind::TimeShift => {
                let config = serde_json::from_value::<TimeShiftConfig>(
                    normalized_processor_config(&processor.config),
                )
                .map_err(|error| ProfileError::InvalidTimeShiftConfig {
                    processor_id: processor.id.clone(),
                    reason: error.to_string(),
                })?;
                validate_time_shift_config(&config, &processor.id)?;
            }
            _ => {}
        }
    }

    let processors = profile
        .processors
        .iter()
        .map(|processor| processor.id.as_str())
        .collect::<BTreeSet<_>>();

    for chain in &profile.processor_chains {
        for processor_id in &chain.processors {
            if !processors.contains(processor_id.as_str()) {
                return Err(ProfileError::UnknownProcessorInChain {
                    chain_id: chain.id.clone(),
                    processor_id: processor_id.clone(),
                });
            }
        }
    }

    Ok(())
}

fn validate_sinks(profile: &Profile) -> Result<(), ProfileError> {
    for file in &profile.sinks.files {
        if file.path.trim().is_empty() {
            return Err(ProfileError::EmptyFileSinkPath {
                sink_id: file.id.clone(),
            });
        }
    }
    for stream in &profile.sinks.streams {
        if stream.endpoint.trim().is_empty() {
            return Err(ProfileError::EmptyStreamSinkEndpoint {
                sink_id: stream.id.clone(),
            });
        }
    }
    Ok(())
}

fn collect_node_channels(profile: &Profile) -> BTreeMap<&str, u16> {
    let default_channels = profile.audio.channels.as_value().unwrap_or(2);
    let mut map = BTreeMap::new();

    for output in &profile.virtual_devices.outputs {
        map.insert(
            output.id.as_str(),
            output.channels.unwrap_or(default_channels),
        );
    }
    for input in &profile.virtual_devices.inputs {
        map.insert(
            input.id.as_str(),
            input.channels.unwrap_or(default_channels),
        );
    }
    for bus in &profile.buses {
        map.insert(bus.id.as_str(), bus.channels.unwrap_or(default_channels));
    }
    for input in &profile.external.inputs {
        map.insert(
            input.id.as_str(),
            input.channels.unwrap_or(default_channels),
        );
    }
    for output in &profile.external.outputs {
        map.insert(
            output.id.as_str(),
            output.channels.unwrap_or(default_channels),
        );
    }

    map
}

fn validate_external_matchers(profile: &Profile) -> Result<(), ProfileError> {
    for endpoint in &profile.external.inputs {
        if let Some(pattern) = endpoint.r#match.name_regex.as_deref() {
            Regex::new(pattern).map_err(|error| ProfileError::InvalidNameRegex {
                id: endpoint.id.clone(),
                pattern: pattern.to_string(),
                reason: error.to_string(),
            })?;
        }
    }

    for endpoint in &profile.external.outputs {
        if let Some(pattern) = endpoint.r#match.name_regex.as_deref() {
            Regex::new(pattern).map_err(|error| ProfileError::InvalidNameRegex {
                id: endpoint.id.clone(),
                pattern: pattern.to_string(),
                reason: error.to_string(),
            })?;
        }
    }

    Ok(())
}

fn validate_ids(profile: &Profile) -> Result<(), ProfileError> {
    let mut all = BTreeSet::<String>::new();

    for id in profile
        .virtual_devices
        .outputs
        .iter()
        .map(|item| item.id.as_str())
        .chain(
            profile
                .virtual_devices
                .inputs
                .iter()
                .map(|item| item.id.as_str()),
        )
        .chain(profile.buses.iter().map(|item| item.id.as_str()))
        .chain(profile.external.inputs.iter().map(|item| item.id.as_str()))
        .chain(profile.external.outputs.iter().map(|item| item.id.as_str()))
        .chain(profile.routes.iter().map(|item| item.id.as_str()))
        .chain(profile.processors.iter().map(|item| item.id.as_str()))
        .chain(profile.processor_chains.iter().map(|item| item.id.as_str()))
        .chain(
            profile
                .captures
                .process_taps
                .iter()
                .map(|item| item.id.as_str()),
        )
        .chain(
            profile
                .captures
                .system_taps
                .iter()
                .map(|item| item.id.as_str()),
        )
        .chain(profile.sinks.files.iter().map(|item| item.id.as_str()))
        .chain(profile.sinks.streams.iter().map(|item| item.id.as_str()))
    {
        if !valid_id(id) {
            return Err(ProfileError::InvalidId { id: id.to_string() });
        }

        if !all.insert(id.to_string()) {
            return Err(ProfileError::DuplicateId { id: id.to_string() });
        }
    }

    for tap in &profile.captures.process_taps {
        if let mars_types::ProcessTapSelector::BundleId { bundle_id } = &tap.selector {
            if bundle_id.trim().is_empty() {
                return Err(ProfileError::InvalidId { id: tap.id.clone() });
            }
        }
    }

    Ok(())
}

fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }

    let mut len = 1usize;
    for ch in chars {
        len += 1;
        if len > 64 {
            return false;
        }
        if !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            return false;
        }
    }

    true
}

fn default_template(name: &str) -> String {
    format!(
        r#"version: 2
name: "{name}"
audio:
  sample_rate: 48000
  channels: 2
  buffer_frames: 256
  format: f32
  latency_mode: balanced

virtual:
  outputs:
    - id: bus-1
      name: "Bus: App"
  inputs:
    - id: mix-1
      name: "Mix: Main"

routes:
  - id: route-app-main
    from: bus-1
    to: mix-1
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]

processors: []

processor_chains: []

captures:
  process_taps: []
  system_taps: []

sinks:
  files: []
  streams: []

pipes:
  - from: bus-1
    to: mix-1
"#
    )
}

fn multi_template(name: &str) -> String {
    format!(
        r#"version: 2
name: "{name}"
audio:
  sample_rate: 48000
  channels: 2
  buffer_frames: 256

virtual:
  outputs:
    - id: app-browser
      name: "Bus: Browser"
    - id: app-music
      name: "Bus: Music"
  inputs:
    - id: mix-main
      name: "Mix: Main"

buses:
  - id: merge-bus
    channels: 2
    mix:
      limiter: true
      limit_dbfs: -1.0
      mode: sum

external:
  outputs:
    - id: monitor
      match:
        name_regex: ".*Speakers.*"

routes:
  - id: route-merge-main
    from: merge-bus
    to: mix-main
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]

processors: []

processor_chains: []

captures:
  process_taps: []
  system_taps: []

sinks:
  files: []
  streams: []

pipes:
  - from: app-browser
    to: merge-bus
    gain_db: -6.0
  - from: app-music
    to: merge-bus
  - from: merge-bus
    to: mix-main
  - from: merge-bus
    to: monitor
    gain_db: -3.0
"#
    )
}

fn blank_template(name: &str) -> String {
    format!(
        r#"version: 2
name: "{name}"
audio:
  sample_rate: 48000
  channels: 2
  buffer_frames: 256

virtual:
  outputs: []
  inputs: []

buses: []

external:
  inputs: []
  outputs: []

routes: []

processors: []

processor_chains: []

captures:
  process_taps: []
  system_taps: []

sinks:
  files: []
  streams: []

pipes: []

policy:
  on_missing_external: error
  apply_mode: atomic
"#
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{parse_profile_str, validate_profile};

    #[test]
    fn validates_default_profile() {
        let yaml = r#"
version: 2
audio:
  sample_rate: 48000
  channels: 2
  buffer_frames: 256
virtual:
  outputs:
    - id: app1
      name: App 1
  inputs:
    - id: mic1
      name: Mic 1
pipes:
  - from: app1
    to: mic1
"#;

        let profile = parse_profile_str(yaml).expect("parse should work");
        let validated = validate_profile(profile).expect("validation should pass");
        assert_eq!(validated.graph.edges.len(), 1);
    }

    #[test]
    fn rejects_invalid_id() {
        let yaml = r#"
version: 2
virtual:
  outputs:
    - id: "bad id"
      name: App
  inputs: []
pipes: []
"#;

        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("invalid id"));
    }

    #[test]
    fn rejects_invalid_external_name_regex() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
external:
  outputs:
    - id: monitor
      match:
        name_regex: "*("
pipes: []
"#;

        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("invalid name_regex"));
    }

    #[test]
    fn accepts_valid_external_name_regex() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
external:
  outputs:
    - id: monitor
      match:
        name_regex: ".*Mic.*"
pipes: []
"#;

        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let validated = validate_profile(profile).expect("validation should pass");
        assert_eq!(validated.graph.edges.len(), 0);
    }

    #[test]
    fn rejects_legacy_on_missing_override_field() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
external:
  outputs:
    - id: monitor
      match:
        name: "Speaker"
      on_missing: error
pipes: []
"#;
        let err = parse_profile_str(yaml).expect_err("yaml parse must fail");
        assert!(err.to_string().contains("on_missing"));
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_legacy_fallback_matcher_field() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
external:
  inputs:
    - id: mic
      match:
        name: "Mic"
      fallback:
        name: "Built-in Microphone"
pipes: []
"#;
        let err = parse_profile_str(yaml).expect_err("yaml parse must fail");
        assert!(err.to_string().contains("fallback"));
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_legacy_apply_mode_best_effort() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
pipes: []
policy:
  apply_mode: best_effort
"#;
        let err = parse_profile_str(yaml).expect_err("yaml parse must fail");
        assert!(err.to_string().contains("invalid yaml"));
        assert!(err.to_string().contains("best_effort"));
    }

    #[test]
    fn rejects_legacy_on_missing_external_skip() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
pipes: []
policy:
  on_missing_external: skip
"#;
        let err = parse_profile_str(yaml).expect_err("yaml parse must fail");
        assert!(err.to_string().contains("invalid yaml"));
        assert!(err.to_string().contains("skip"));
    }

    #[test]
    fn parser_rejects_v1_profile() {
        let yaml = r#"
version: 1
virtual:
  outputs: []
  inputs: []
pipes: []
"#;
        let err = parse_profile_str(yaml).expect_err("v1 must be rejected");
        assert!(err.to_string().contains("unsupported profile version"));
        assert!(err.to_string().contains('1'));
    }

    #[test]
    fn rejects_route_matrix_dimension_mismatch() {
        let yaml = r#"
version: 2
audio:
  channels: 2
virtual:
  outputs:
    - id: app1
      name: App 1
      channels: 2
  inputs:
    - id: mix1
      name: Mix 1
      channels: 2
routes:
  - id: route1
    from: app1
    to: mix1
    matrix:
      rows: 1
      cols: 2
      coefficients:
        - [1.0, 0.0]
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("matrix channels mismatch"));
    }

    #[test]
    fn rejects_route_with_missing_reference() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs:
    - id: mix1
      name: Mix 1
routes:
  - id: route1
    from: missing
    to: mix1
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("unknown source"));
    }

    #[test]
    fn rejects_unknown_processor_reference_in_chain() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
processors: []
processor_chains:
  - id: chain1
    processors:
      - missing-processor
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("unknown processor"));
    }

    #[test]
    fn rejects_unknown_route_chain_reference() {
        let yaml = r#"
version: 2
virtual:
  outputs:
    - id: app1
      name: App 1
  inputs:
    - id: mix1
      name: Mix 1
routes:
  - id: route1
    from: app1
    to: mix1
    chain: missing-chain
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("unknown processor chain"));
    }

    #[test]
    fn rejects_invalid_processor_enum_value() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
processors:
  - id: p1
    kind: not_a_processor
pipes: []
"#;
        let err = parse_profile_str(yaml).expect_err("yaml parse must fail");
        assert!(err.to_string().contains("invalid yaml"));
        assert!(err.to_string().contains("not_a_processor"));
    }

    #[test]
    fn accepts_dsp_blocks_with_default_null_config() {
        let yaml = r#"
version: 2
virtual:
  outputs:
    - id: app
      name: App
  inputs:
    - id: mix
      name: Mix
processors:
  - id: eq1
    kind: eq
  - id: dyn1
    kind: dynamics
  - id: den1
    kind: denoise
  - id: ts1
    kind: time_shift
processor_chains:
  - id: chain1
    processors: [eq1, dyn1, den1, ts1]
routes:
  - id: r1
    from: app
    to: mix
    chain: chain1
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        validate_profile(profile).expect("validation should pass");
    }

    #[test]
    fn rejects_invalid_eq_config_range() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
processors:
  - id: eq1
    kind: eq
    config:
      bands:
        - freq_hz: 1000.0
          q: 0.0
          gain_db: 3.0
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("eq config is invalid"));
        assert!(err.to_string().contains("q must be finite in (0, 24]"));
    }

    #[test]
    fn rejects_invalid_dynamics_config_range() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
processors:
  - id: dyn1
    kind: dynamics
    config:
      threshold_db: -12.0
      ratio: 0.5
      attack_ms: 5.0
      release_ms: 80.0
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("dynamics config is invalid"));
        assert!(err.to_string().contains("ratio must be finite in [1, 32]"));
    }

    #[test]
    fn rejects_invalid_denoise_config_range() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
processors:
  - id: den1
    kind: denoise
    config:
      threshold_db: -30.0
      reduction_db: 61.0
      attack_ms: 5.0
      release_ms: 120.0
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("denoise config is invalid"));
        assert!(
            err.to_string()
                .contains("reduction_db must be finite in [0, 60]")
        );
    }

    #[test]
    fn rejects_invalid_time_shift_config_range() {
        let yaml = r#"
version: 2
virtual:
  outputs: []
  inputs: []
processors:
  - id: ts1
    kind: time_shift
    config:
      delay_ms: 900.0
      max_delay_ms: 500.0
pipes: []
"#;
        let profile = parse_profile_str(yaml).expect("yaml parse should work");
        let err = validate_profile(profile).expect_err("validation must fail");
        assert!(err.to_string().contains("time-shift config is invalid"));
        assert!(err.to_string().contains("delay_ms must be <= max_delay_ms"));
    }
}

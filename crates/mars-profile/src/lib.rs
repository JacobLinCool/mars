#![forbid(unsafe_code)]
//! YAML profile parsing, schema generation, and semantic validation.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use mars_graph::{GraphError, RoutingGraph, build_routing_graph};
use mars_types::{AutoOrU16, AutoOrU32, PROFILE_VERSION, Profile, ValidationReport};
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
    serde_yaml::from_str(raw).map_err(ProfileError::Yaml)
}

pub fn validate_profile(profile: Profile) -> Result<ValidatedProfile, ProfileError> {
    if profile.version != PROFILE_VERSION {
        return Err(ProfileError::UnsupportedVersion(profile.version));
    }

    validate_audio(&profile)?;
    validate_ids(&profile)?;
    validate_pipe_ranges(&profile)?;
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
    }
    Ok(())
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
    {
        if !valid_id(id) {
            return Err(ProfileError::InvalidId { id: id.to_string() });
        }

        if !all.insert(id.to_string()) {
            return Err(ProfileError::DuplicateId { id: id.to_string() });
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
        r#"version: 1
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

pipes:
  - from: bus-1
    to: mix-1
"#
    )
}

fn multi_template(name: &str) -> String {
    format!(
        r#"version: 1
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
        r#"version: 1
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
version: 1
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
version: 1
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
version: 1
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
version: 1
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
version: 1
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
version: 1
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
version: 1
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
version: 1
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
}

#![forbid(unsafe_code)]
//! Realtime-safe-ish audio graph rendering engine.

mod au_host;

use std::collections::{BTreeMap, HashMap};
use std::f32::consts::{FRAC_PI_4, PI};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arc_swap::ArcSwap;
use au_host::{AuProcessRequest, AuSubmitError, AuWorker, AuWorkerSettings};
use mars_graph::RoutingGraph;
use mars_types::{
    AuPluginConfig, MixMode, PluginHostHealth, PluginHostInstanceStatus, PluginHostRuntimeStatus,
    ProcessorKind, ProcessorRuntimeStats, RuntimeCounters,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    pub graph: RoutingGraph,
    pub sample_rate: u32,
    pub buffer_frames: u32,
}

#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    pub sinks: HashMap<String, Vec<f32>>,
    pub counters: RuntimeCounters,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("render frames must be > 0")]
    InvalidFrames,
    #[error("source '{id}' sample count is not aligned with channels {channels}")]
    InvalidSourceSampleCount { id: String, channels: u16 },
}

#[derive(Debug)]
pub struct Engine {
    snapshot: ArcSwap<EngineSnapshot>,
    state: Mutex<EngineState>,
    processor_schedule: ArcSwap<ProcessorSchedule>,
    processor_controls: ArcSwap<ProcessorControlSnapshot>,
}

#[derive(Debug, Default)]
struct EngineState {
    routes: Vec<RouteRuntime>,
    outgoing_routes: HashMap<String, Vec<usize>>,
    sink_nodes: Vec<String>,
    sink_contributions: HashMap<String, usize>,
    node_buffers: HashMap<String, Vec<f32>>,
    edge_scratch: Vec<f32>,
    counters: RuntimeCounters,
}

#[derive(Debug)]
struct RouteRuntime {
    id: String,
    from: String,
    to: String,
    source_channels: usize,
    destination_channels: usize,
    matrix: Vec<f32>,
    gain_db: f32,
    mute: bool,
    pan: f32,
    delay_line: DelayLine,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessorControl {
    pub bypass: bool,
    pub generation: u64,
    pub params: BTreeMap<String, f32>,
}

pub type ProcessorControlSnapshot = BTreeMap<String, ProcessorControl>;

#[derive(Debug, Clone, Default)]
pub struct ProcessorSchedule {
    route_chains: BTreeMap<String, ProcessorRouteChain>,
}

#[derive(Debug, Clone)]
struct ProcessorRouteChain {
    processors: Vec<Arc<dyn ProcessorBlock>>,
}

trait ProcessorBlock: Send + Sync + std::fmt::Debug {
    fn id(&self) -> &str;
    fn prepare(&self, context: ProcessorPrepareContext);
    fn process(
        &self,
        samples: &mut [f32],
        channels: usize,
        frames: usize,
        control: Option<&ProcessorControl>,
        bypass: bool,
    );
    fn reset(&self);
    fn stats(&self) -> ProcessorRuntimeStats;
    fn plugin_status(&self) -> Option<PluginHostInstanceStatus> {
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcessorPrepareContext {
    channels: usize,
    sample_rate: u32,
    max_frames: usize,
}

#[derive(Debug, Default)]
struct ProcessorCounters {
    prepare_calls: AtomicU64,
    process_calls: AtomicU64,
    reset_calls: AtomicU64,
    last_generation: AtomicU64,
}

#[derive(Debug)]
struct EqProcessorBlock {
    id: String,
    config: EqConfig,
    state: Mutex<EqProcessorState>,
    prepared: AtomicBool,
    counters: ProcessorCounters,
}

#[derive(Debug)]
struct DynamicsProcessorBlock {
    id: String,
    config: DynamicsConfig,
    state: Mutex<DynamicsProcessorState>,
    prepared: AtomicBool,
    counters: ProcessorCounters,
}

#[derive(Debug)]
struct DenoiseProcessorBlock {
    id: String,
    config: DenoiseConfig,
    state: Mutex<DenoiseProcessorState>,
    prepared: AtomicBool,
    counters: ProcessorCounters,
}

#[derive(Debug)]
struct TimeShiftProcessorBlock {
    id: String,
    config: TimeShiftConfig,
    state: Mutex<TimeShiftProcessorState>,
    prepared: AtomicBool,
    counters: ProcessorCounters,
}

#[derive(Debug)]
struct AuProcessorBlock {
    id: String,
    config: AuPluginConfig,
    state: Mutex<AuProcessorState>,
    prepared: AtomicBool,
    counters: ProcessorCounters,
}

#[derive(Debug, Default)]
struct AuProcessorState {
    worker: Option<AuWorker>,
    channels: usize,
    sample_rate_hz: u32,
    max_frames: usize,
}

#[derive(Debug, Clone)]
struct EqConfig {
    bands: Vec<EqBandConfig>,
}

#[derive(Debug, Clone, Copy)]
struct EqBandConfig {
    freq_hz: f32,
    q: f32,
    gain_db: f32,
    enabled: bool,
}

#[derive(Debug, Clone, Copy)]
struct DynamicsConfig {
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    makeup_gain_db: f32,
    limiter: bool,
}

#[derive(Debug, Clone, Copy)]
struct DenoiseConfig {
    threshold_db: f32,
    reduction_db: f32,
    attack_ms: f32,
    release_ms: f32,
}

#[derive(Debug, Clone, Copy)]
struct TimeShiftConfig {
    delay_ms: f32,
    max_delay_ms: f32,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct EqConfigSerde {
    bands: Vec<EqBandConfigSerde>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EqBandConfigSerde {
    freq_hz: f32,
    q: f32,
    gain_db: f32,
    enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DynamicsConfigSerde {
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    makeup_gain_db: f32,
    limiter: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DenoiseConfigSerde {
    threshold_db: f32,
    reduction_db: f32,
    attack_ms: f32,
    release_ms: f32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TimeShiftConfigSerde {
    delay_ms: f32,
    max_delay_ms: f32,
}

#[derive(Debug, Clone, Copy)]
struct BiquadCoefficients {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

#[derive(Debug, Clone, Copy, Default)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

#[derive(Debug, Clone)]
struct EqBandRuntime {
    enabled: bool,
    coefficients: BiquadCoefficients,
    channel_state: Vec<BiquadState>,
}

#[derive(Debug, Clone, Default)]
struct EqProcessorState {
    bands: Vec<EqBandRuntime>,
}

#[derive(Debug, Clone, Default)]
struct DynamicsProcessorState {
    envelope: Vec<f32>,
    gain: Vec<f32>,
    sample_rate_hz: u32,
}

#[derive(Debug, Clone, Default)]
struct DenoiseProcessorState {
    envelope: Vec<f32>,
    gain: Vec<f32>,
    sample_rate_hz: u32,
}

#[derive(Debug, Clone, Default)]
struct TimeShiftProcessorState {
    ring: Vec<f32>,
    ring_frames: usize,
    write_frame: usize,
    delay_frames: usize,
    channels: usize,
}

#[derive(Debug, Clone)]
struct DelayLine {
    data: Vec<f32>,
    write_idx: usize,
    delay_frames: usize,
}

impl DelayLine {
    fn new(delay_frames: usize, channels: usize) -> Self {
        let len = delay_frames.saturating_mul(channels);
        Self {
            data: vec![0.0; len],
            write_idx: 0,
            delay_frames,
        }
    }

    fn process_in_place(&mut self, samples: &mut [f32]) {
        if self.delay_frames == 0 || self.data.is_empty() {
            return;
        }

        for sample in samples.iter_mut() {
            std::mem::swap(&mut self.data[self.write_idx], &mut *sample);
            self.write_idx = (self.write_idx + 1) % self.data.len();
        }
    }
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

impl Default for EqBandConfigSerde {
    fn default() -> Self {
        Self {
            freq_hz: default_eq_freq_hz(),
            q: default_eq_q(),
            gain_db: 0.0,
            enabled: true,
        }
    }
}

impl Default for DynamicsConfigSerde {
    fn default() -> Self {
        Self {
            threshold_db: default_dynamics_threshold_db(),
            ratio: default_dynamics_ratio(),
            attack_ms: default_dynamics_attack_ms(),
            release_ms: default_dynamics_release_ms(),
            makeup_gain_db: 0.0,
            limiter: false,
        }
    }
}

impl Default for DenoiseConfigSerde {
    fn default() -> Self {
        Self {
            threshold_db: default_denoise_threshold_db(),
            reduction_db: default_denoise_reduction_db(),
            attack_ms: default_denoise_attack_ms(),
            release_ms: default_denoise_release_ms(),
        }
    }
}

impl Default for TimeShiftConfigSerde {
    fn default() -> Self {
        Self {
            delay_ms: 0.0,
            max_delay_ms: default_timeshift_max_delay_ms(),
        }
    }
}

impl EqConfig {
    fn from_config_json(config_json: &str) -> Self {
        let value =
            serde_json::from_str::<Value>(config_json).expect("compiled config must be valid JSON");
        let parsed = if value.is_null() {
            EqConfigSerde::default()
        } else {
            serde_json::from_value::<EqConfigSerde>(value)
                .expect("compiled eq config must match validated shape")
        };
        let bands = parsed
            .bands
            .into_iter()
            .map(|band| EqBandConfig {
                freq_hz: band.freq_hz,
                q: band.q,
                gain_db: band.gain_db,
                enabled: band.enabled,
            })
            .collect::<Vec<_>>();
        Self { bands }
    }
}

impl DynamicsConfig {
    fn from_config_json(config_json: &str) -> Self {
        let value =
            serde_json::from_str::<Value>(config_json).expect("compiled config must be valid JSON");
        let parsed = if value.is_null() {
            DynamicsConfigSerde::default()
        } else {
            serde_json::from_value::<DynamicsConfigSerde>(value)
                .expect("compiled dynamics config must match validated shape")
        };

        Self {
            threshold_db: parsed.threshold_db,
            ratio: parsed.ratio,
            attack_ms: parsed.attack_ms,
            release_ms: parsed.release_ms,
            makeup_gain_db: parsed.makeup_gain_db,
            limiter: parsed.limiter,
        }
    }
}

impl DenoiseConfig {
    fn from_config_json(config_json: &str) -> Self {
        let value =
            serde_json::from_str::<Value>(config_json).expect("compiled config must be valid JSON");
        let parsed = if value.is_null() {
            DenoiseConfigSerde::default()
        } else {
            serde_json::from_value::<DenoiseConfigSerde>(value)
                .expect("compiled denoise config must match validated shape")
        };

        Self {
            threshold_db: parsed.threshold_db,
            reduction_db: parsed.reduction_db,
            attack_ms: parsed.attack_ms,
            release_ms: parsed.release_ms,
        }
    }
}

impl TimeShiftConfig {
    fn from_config_json(config_json: &str) -> Self {
        let value =
            serde_json::from_str::<Value>(config_json).expect("compiled config must be valid JSON");
        let parsed = if value.is_null() {
            TimeShiftConfigSerde::default()
        } else {
            serde_json::from_value::<TimeShiftConfigSerde>(value)
                .expect("compiled time-shift config must match validated shape")
        };

        Self {
            delay_ms: parsed.delay_ms,
            max_delay_ms: parsed.max_delay_ms,
        }
    }
}

fn au_config_from_json(config_json: &str) -> AuPluginConfig {
    let value =
        serde_json::from_str::<Value>(config_json).expect("compiled config must be valid JSON");
    if value.is_null() {
        return AuPluginConfig::default();
    }
    serde_json::from_value::<AuPluginConfig>(value)
        .expect("compiled au config must match validated shape")
}

fn peaking_coefficients(
    sample_rate_hz: u32,
    freq_hz: f32,
    q: f32,
    gain_db: f32,
) -> BiquadCoefficients {
    let sample_rate = sample_rate_hz.max(1) as f32;
    let omega = 2.0 * PI * (freq_hz / sample_rate);
    let sin_omega = omega.sin();
    let cos_omega = omega.cos();
    let alpha = sin_omega / (2.0 * q.max(0.0001));
    let a = 10.0_f32.powf(gain_db / 40.0);

    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * cos_omega;
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * cos_omega;
    let a2 = 1.0 - alpha / a;

    BiquadCoefficients {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn process_biquad_sample(
    coefficients: BiquadCoefficients,
    sample: f32,
    state: &mut BiquadState,
) -> f32 {
    let output = coefficients.b0 * sample + coefficients.b1 * state.x1 + coefficients.b2 * state.x2
        - coefficients.a1 * state.y1
        - coefficients.a2 * state.y2;
    state.x2 = state.x1;
    state.x1 = sample;
    state.y2 = state.y1;
    state.y1 = output;
    output
}

impl ProcessorSchedule {
    #[must_use]
    pub fn from_snapshot(snapshot: &EngineSnapshot) -> Self {
        let mut route_chains = BTreeMap::<String, ProcessorRouteChain>::new();

        for route in &snapshot.graph.compiled_route_plan().routes {
            let Some(chain_id) = route.chain.as_ref() else {
                continue;
            };
            let Some(compiled_chain) = snapshot.graph.processor_plan().chains.get(chain_id) else {
                continue;
            };

            let mut processors =
                Vec::<Arc<dyn ProcessorBlock>>::with_capacity(compiled_chain.processors.len());
            let context = ProcessorPrepareContext {
                channels: route.destination_channels as usize,
                sample_rate: snapshot.sample_rate,
                max_frames: snapshot.buffer_frames as usize,
            };
            for compiled in &compiled_chain.processors {
                let processor: Arc<dyn ProcessorBlock> = match compiled.kind {
                    ProcessorKind::Eq => Arc::new(EqProcessorBlock::new(
                        compiled.id.clone(),
                        EqConfig::from_config_json(&compiled.config_json),
                    )),
                    ProcessorKind::Dynamics => Arc::new(DynamicsProcessorBlock::new(
                        compiled.id.clone(),
                        DynamicsConfig::from_config_json(&compiled.config_json),
                    )),
                    ProcessorKind::Denoise => Arc::new(DenoiseProcessorBlock::new(
                        compiled.id.clone(),
                        DenoiseConfig::from_config_json(&compiled.config_json),
                    )),
                    ProcessorKind::TimeShift => Arc::new(TimeShiftProcessorBlock::new(
                        compiled.id.clone(),
                        TimeShiftConfig::from_config_json(&compiled.config_json),
                    )),
                    ProcessorKind::Au => Arc::new(AuProcessorBlock::new(
                        compiled.id.clone(),
                        au_config_from_json(&compiled.config_json),
                    )),
                };
                processor.prepare(context);
                processors.push(processor);
            }

            route_chains.insert(route.id.clone(), ProcessorRouteChain { processors });
        }

        Self { route_chains }
    }

    fn process_route(
        &self,
        route_id: &str,
        samples: &mut [f32],
        channels: usize,
        frames: usize,
        controls: &ProcessorControlSnapshot,
    ) {
        let Some(chain) = self.route_chains.get(route_id) else {
            return;
        };
        chain.process(samples, channels, frames, controls);
    }

    fn stats(&self) -> BTreeMap<String, ProcessorRuntimeStats> {
        let mut merged = BTreeMap::<String, ProcessorRuntimeStats>::new();
        for chain in self.route_chains.values() {
            for processor in &chain.processors {
                let current = processor.stats();
                let entry = merged.entry(processor.id().to_string()).or_default();
                entry.prepare_calls += current.prepare_calls;
                entry.process_calls += current.process_calls;
                entry.reset_calls += current.reset_calls;
                entry.last_generation = entry.last_generation.max(current.last_generation);
            }
        }
        merged
    }

    fn plugin_runtime_status(&self) -> PluginHostRuntimeStatus {
        let mut runtime = PluginHostRuntimeStatus::default();
        for chain in self.route_chains.values() {
            for processor in &chain.processors {
                let Some(status) = processor.plugin_status() else {
                    continue;
                };
                runtime.timeout_count = runtime.timeout_count.saturating_add(status.timeout_count);
                runtime.error_count = runtime.error_count.saturating_add(status.error_count);
                runtime.restart_count = runtime.restart_count.saturating_add(status.restart_count);
                if status.loaded && status.health != PluginHostHealth::Failed {
                    runtime.active_instances = runtime.active_instances.saturating_add(1);
                }
                if status.health == PluginHostHealth::Failed {
                    runtime.failed_instances = runtime.failed_instances.saturating_add(1);
                }
                runtime.instances.push(status);
            }
        }
        runtime
    }
}

impl Drop for ProcessorSchedule {
    fn drop(&mut self) {
        for chain in self.route_chains.values() {
            chain.reset();
        }
    }
}

impl ProcessorRouteChain {
    fn process(
        &self,
        samples: &mut [f32],
        channels: usize,
        frames: usize,
        controls: &ProcessorControlSnapshot,
    ) {
        for processor in &self.processors {
            let control = controls.get(processor.id());
            let bypass = control.is_some_and(|item| item.bypass);
            processor.process(samples, channels, frames, control, bypass);
        }
    }

    fn reset(&self) {
        for processor in &self.processors {
            processor.reset();
        }
    }
}

impl EqProcessorBlock {
    fn new(id: String, config: EqConfig) -> Self {
        Self {
            id,
            config,
            state: Mutex::new(EqProcessorState::default()),
            prepared: AtomicBool::new(false),
            counters: ProcessorCounters::default(),
        }
    }
}

impl ProcessorBlock for EqProcessorBlock {
    fn id(&self) -> &str {
        &self.id
    }

    fn prepare(&self, context: ProcessorPrepareContext) {
        let mut state = self.state.lock();
        state.bands.clear();
        for band in &self.config.bands {
            state.bands.push(EqBandRuntime {
                enabled: band.enabled,
                coefficients: peaking_coefficients(
                    context.sample_rate,
                    band.freq_hz,
                    band.q,
                    band.gain_db,
                ),
                channel_state: vec![BiquadState::default(); context.channels],
            });
        }
        self.prepared.store(true, Ordering::Relaxed);
        self.counters.prepare_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn process(
        &self,
        samples: &mut [f32],
        channels: usize,
        _frames: usize,
        control: Option<&ProcessorControl>,
        bypass: bool,
    ) {
        if bypass || !self.prepared.load(Ordering::Relaxed) {
            return;
        }
        if let Some(control) = control {
            self.counters
                .last_generation
                .store(control.generation, Ordering::Relaxed);
        }

        let mut state = self.state.lock();
        if state.bands.is_empty() || channels == 0 {
            self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
            return;
        }
        for frame in samples.chunks_exact_mut(channels) {
            for (channel_index, sample) in frame.iter_mut().enumerate() {
                let mut output = *sample;
                for band in &mut state.bands {
                    if !band.enabled {
                        continue;
                    }
                    let Some(channel_state) = band.channel_state.get_mut(channel_index) else {
                        continue;
                    };
                    output = process_biquad_sample(band.coefficients, output, channel_state);
                }
                *sample = output;
            }
        }
        self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        if self.prepared.swap(false, Ordering::Relaxed) {
            self.state.lock().bands.clear();
            self.counters.reset_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stats(&self) -> ProcessorRuntimeStats {
        ProcessorRuntimeStats {
            prepare_calls: self.counters.prepare_calls.load(Ordering::Relaxed),
            process_calls: self.counters.process_calls.load(Ordering::Relaxed),
            reset_calls: self.counters.reset_calls.load(Ordering::Relaxed),
            last_generation: self.counters.last_generation.load(Ordering::Relaxed),
        }
    }
}

impl DynamicsProcessorBlock {
    fn new(id: String, config: DynamicsConfig) -> Self {
        Self {
            id,
            config,
            state: Mutex::new(DynamicsProcessorState::default()),
            prepared: AtomicBool::new(false),
            counters: ProcessorCounters::default(),
        }
    }
}

impl ProcessorBlock for DynamicsProcessorBlock {
    fn id(&self) -> &str {
        &self.id
    }

    fn prepare(&self, context: ProcessorPrepareContext) {
        let mut state = self.state.lock();
        state.envelope = vec![0.0; context.channels];
        state.gain = vec![1.0; context.channels];
        state.sample_rate_hz = context.sample_rate.max(1);
        self.prepared.store(true, Ordering::Relaxed);
        self.counters.prepare_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn process(
        &self,
        samples: &mut [f32],
        channels: usize,
        _frames: usize,
        control: Option<&ProcessorControl>,
        bypass: bool,
    ) {
        if bypass || !self.prepared.load(Ordering::Relaxed) {
            return;
        }
        if channels == 0 {
            return;
        }
        if let Some(control) = control {
            self.counters
                .last_generation
                .store(control.generation, Ordering::Relaxed);
        }

        let mut state = self.state.lock();
        if state.envelope.len() != channels {
            state.envelope.resize(channels, 0.0);
            state.gain.resize(channels, 1.0);
        }

        let sample_rate = state.sample_rate_hz.max(1) as f32;
        let attack_coeff = (-1.0 / (self.config.attack_ms.max(0.1) * 0.001 * sample_rate)).exp();
        let release_coeff = (-1.0 / (self.config.release_ms.max(1.0) * 0.001 * sample_rate)).exp();
        let makeup_gain = 10.0_f32.powf(self.config.makeup_gain_db / 20.0);

        for frame in samples.chunks_exact_mut(channels) {
            for (channel_index, sample) in frame.iter_mut().enumerate() {
                let input = *sample;
                let level = input.abs().max(1e-12);
                let current_env = state.envelope[channel_index];
                let env_coeff = if level > current_env {
                    attack_coeff
                } else {
                    release_coeff
                };
                let envelope = env_coeff * current_env + (1.0 - env_coeff) * level;
                state.envelope[channel_index] = envelope;
                let level_db = 20.0 * envelope.max(1e-12).log10();
                let over_db = (level_db - self.config.threshold_db).max(0.0);
                let compressed_over_db = over_db / self.config.ratio.max(1.0);
                let gain_reduction_db = compressed_over_db - over_db;
                let target_gain = 10.0_f32.powf(gain_reduction_db / 20.0) * makeup_gain;

                let current_gain = state.gain[channel_index];
                let coeff = if target_gain < current_gain {
                    attack_coeff
                } else {
                    release_coeff
                };
                let smoothed_gain = coeff * current_gain + (1.0 - coeff) * target_gain;
                state.gain[channel_index] = smoothed_gain;

                let mut output = input * smoothed_gain;
                if self.config.limiter {
                    output = output.clamp(-1.0, 1.0);
                }
                *sample = output;
            }
        }

        self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        if self.prepared.swap(false, Ordering::Relaxed) {
            let mut state = self.state.lock();
            state.envelope.clear();
            state.gain.clear();
            self.counters.reset_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stats(&self) -> ProcessorRuntimeStats {
        ProcessorRuntimeStats {
            prepare_calls: self.counters.prepare_calls.load(Ordering::Relaxed),
            process_calls: self.counters.process_calls.load(Ordering::Relaxed),
            reset_calls: self.counters.reset_calls.load(Ordering::Relaxed),
            last_generation: self.counters.last_generation.load(Ordering::Relaxed),
        }
    }
}

impl DenoiseProcessorBlock {
    fn new(id: String, config: DenoiseConfig) -> Self {
        Self {
            id,
            config,
            state: Mutex::new(DenoiseProcessorState::default()),
            prepared: AtomicBool::new(false),
            counters: ProcessorCounters::default(),
        }
    }
}

impl ProcessorBlock for DenoiseProcessorBlock {
    fn id(&self) -> &str {
        &self.id
    }

    fn prepare(&self, context: ProcessorPrepareContext) {
        let mut state = self.state.lock();
        state.envelope = vec![0.0; context.channels];
        state.gain = vec![1.0; context.channels];
        state.sample_rate_hz = context.sample_rate.max(1);
        self.prepared.store(true, Ordering::Relaxed);
        self.counters.prepare_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn process(
        &self,
        samples: &mut [f32],
        channels: usize,
        _frames: usize,
        control: Option<&ProcessorControl>,
        bypass: bool,
    ) {
        if bypass || !self.prepared.load(Ordering::Relaxed) {
            return;
        }
        if channels == 0 {
            return;
        }
        if let Some(control) = control {
            self.counters
                .last_generation
                .store(control.generation, Ordering::Relaxed);
        }

        let mut state = self.state.lock();
        if state.envelope.len() != channels {
            state.envelope.resize(channels, 0.0);
            state.gain.resize(channels, 1.0);
        }

        let sample_rate = state.sample_rate_hz.max(1) as f32;
        let attack_coeff = (-1.0 / (self.config.attack_ms.max(0.1) * 0.001 * sample_rate)).exp();
        let release_coeff = (-1.0 / (self.config.release_ms.max(1.0) * 0.001 * sample_rate)).exp();
        let threshold = 10.0_f32.powf(self.config.threshold_db / 20.0).max(1e-9);
        let min_gain = 10.0_f32
            .powf(-self.config.reduction_db.max(0.0) / 20.0)
            .clamp(0.0, 1.0);

        for frame in samples.chunks_exact_mut(channels) {
            for (channel_index, sample) in frame.iter_mut().enumerate() {
                let input = *sample;
                let level = input.abs().max(1e-12);
                let current_env = state.envelope[channel_index];
                let env_coeff = if level > current_env {
                    attack_coeff
                } else {
                    release_coeff
                };
                let envelope = env_coeff * current_env + (1.0 - env_coeff) * level;
                state.envelope[channel_index] = envelope;

                let target_gain = if envelope >= threshold { 1.0 } else { min_gain };
                let current_gain = state.gain[channel_index];
                let coeff = if target_gain > current_gain {
                    attack_coeff
                } else {
                    release_coeff
                };
                let smoothed_gain = coeff * current_gain + (1.0 - coeff) * target_gain;
                state.gain[channel_index] = smoothed_gain;
                *sample = input * smoothed_gain;
            }
        }

        self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        if self.prepared.swap(false, Ordering::Relaxed) {
            let mut state = self.state.lock();
            state.envelope.clear();
            state.gain.clear();
            self.counters.reset_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stats(&self) -> ProcessorRuntimeStats {
        ProcessorRuntimeStats {
            prepare_calls: self.counters.prepare_calls.load(Ordering::Relaxed),
            process_calls: self.counters.process_calls.load(Ordering::Relaxed),
            reset_calls: self.counters.reset_calls.load(Ordering::Relaxed),
            last_generation: self.counters.last_generation.load(Ordering::Relaxed),
        }
    }
}

impl TimeShiftProcessorBlock {
    fn new(id: String, config: TimeShiftConfig) -> Self {
        Self {
            id,
            config,
            state: Mutex::new(TimeShiftProcessorState::default()),
            prepared: AtomicBool::new(false),
            counters: ProcessorCounters::default(),
        }
    }

    fn delay_frames(delay_ms: f32, sample_rate: u32) -> usize {
        ((delay_ms.max(0.0) * 0.001) * sample_rate.max(1) as f32)
            .round()
            .max(0.0) as usize
    }
}

impl ProcessorBlock for TimeShiftProcessorBlock {
    fn id(&self) -> &str {
        &self.id
    }

    fn prepare(&self, context: ProcessorPrepareContext) {
        let channels = context.channels.max(1);
        let max_delay_frames =
            Self::delay_frames(self.config.max_delay_ms.max(1.0), context.sample_rate).max(1);
        let delay_frames = Self::delay_frames(self.config.delay_ms, context.sample_rate);
        let ring_frames = max_delay_frames.saturating_add(1);
        let mut state = self.state.lock();
        state.ring = vec![0.0; ring_frames.saturating_mul(channels)];
        state.ring_frames = ring_frames;
        state.write_frame = 0;
        state.delay_frames = delay_frames.min(max_delay_frames);
        state.channels = channels;
        self.prepared.store(true, Ordering::Relaxed);
        self.counters.prepare_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn process(
        &self,
        samples: &mut [f32],
        channels: usize,
        _frames: usize,
        control: Option<&ProcessorControl>,
        bypass: bool,
    ) {
        if bypass || !self.prepared.load(Ordering::Relaxed) {
            return;
        }
        if channels == 0 {
            return;
        }
        if let Some(control) = control {
            self.counters
                .last_generation
                .store(control.generation, Ordering::Relaxed);
        }

        let mut state = self.state.lock();
        if state.delay_frames == 0 {
            self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if state.channels != channels || state.ring_frames == 0 || state.ring.is_empty() {
            self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
            return;
        }

        for frame in samples.chunks_exact_mut(channels) {
            let read_frame =
                (state.write_frame + state.ring_frames - state.delay_frames) % state.ring_frames;
            let read_base = read_frame.saturating_mul(channels);
            let write_base = state.write_frame.saturating_mul(channels);

            for (channel_index, sample) in frame.iter_mut().enumerate() {
                let input = *sample;
                let delayed = state.ring[read_base + channel_index];
                state.ring[write_base + channel_index] = input;
                *sample = delayed;
            }

            state.write_frame = (state.write_frame + 1) % state.ring_frames;
        }

        self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        if self.prepared.swap(false, Ordering::Relaxed) {
            let mut state = self.state.lock();
            state.ring.clear();
            state.ring_frames = 0;
            state.write_frame = 0;
            state.delay_frames = 0;
            state.channels = 0;
            self.counters.reset_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stats(&self) -> ProcessorRuntimeStats {
        ProcessorRuntimeStats {
            prepare_calls: self.counters.prepare_calls.load(Ordering::Relaxed),
            process_calls: self.counters.process_calls.load(Ordering::Relaxed),
            reset_calls: self.counters.reset_calls.load(Ordering::Relaxed),
            last_generation: self.counters.last_generation.load(Ordering::Relaxed),
        }
    }
}

impl AuProcessorBlock {
    fn new(id: String, config: AuPluginConfig) -> Self {
        Self {
            id,
            config,
            state: Mutex::new(AuProcessorState::default()),
            prepared: AtomicBool::new(false),
            counters: ProcessorCounters::default(),
        }
    }
}

impl ProcessorBlock for AuProcessorBlock {
    fn id(&self) -> &str {
        &self.id
    }

    fn prepare(&self, context: ProcessorPrepareContext) {
        let mut state = self.state.lock();
        if let Some(worker) = state.worker.as_mut() {
            worker.stop();
        }
        state.channels = context.channels.max(1);
        state.sample_rate_hz = context.sample_rate.max(1);
        state.max_frames = context.max_frames.max(1);
        state.worker = Some(AuWorker::start(AuWorkerSettings {
            processor_id: self.id.clone(),
            config: self.config.clone(),
            sample_rate: state.sample_rate_hz,
            channels: state.channels,
            max_frames: state.max_frames,
        }));
        self.prepared.store(true, Ordering::Relaxed);
        self.counters.prepare_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn process(
        &self,
        samples: &mut [f32],
        channels: usize,
        frames: usize,
        control: Option<&ProcessorControl>,
        bypass: bool,
    ) {
        if !self.prepared.load(Ordering::Relaxed) || channels == 0 || frames == 0 {
            return;
        }
        if let Some(control) = control {
            self.counters
                .last_generation
                .store(control.generation, Ordering::Relaxed);
        }

        if bypass {
            self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let state = self.state.lock();
        if let Some(worker) = state.worker.as_ref() {
            let request_samples = samples.to_vec();
            if let Some(processed) = worker.drain_latest_result() {
                if processed.len() == samples.len() {
                    samples.copy_from_slice(&processed);
                }
            }
            let request = AuProcessRequest {
                frames,
                channels,
                samples: request_samples,
            };
            let _ = match worker.try_submit(request) {
                Ok(()) => Ok(()),
                Err(AuSubmitError::Full) | Err(AuSubmitError::Disconnected) => Err(()),
            };
        }
        self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        if self.prepared.swap(false, Ordering::Relaxed) {
            let mut state = self.state.lock();
            if let Some(worker) = state.worker.as_mut() {
                worker.stop();
            }
            state.worker = None;
            state.channels = 0;
            state.sample_rate_hz = 0;
            state.max_frames = 0;
            self.counters.reset_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stats(&self) -> ProcessorRuntimeStats {
        ProcessorRuntimeStats {
            prepare_calls: self.counters.prepare_calls.load(Ordering::Relaxed),
            process_calls: self.counters.process_calls.load(Ordering::Relaxed),
            reset_calls: self.counters.reset_calls.load(Ordering::Relaxed),
            last_generation: self.counters.last_generation.load(Ordering::Relaxed),
        }
    }

    fn plugin_status(&self) -> Option<PluginHostInstanceStatus> {
        let state = self.state.lock();
        let status = if let Some(worker) = state.worker.as_ref() {
            worker.status_snapshot()
        } else {
            PluginHostInstanceStatus {
                id: self.id.clone(),
                api: self.config.api,
                health: if self.prepared.load(Ordering::Relaxed) {
                    PluginHostHealth::Failed
                } else {
                    PluginHostHealth::Degraded
                },
                loaded: false,
                host_pid: None,
                process_calls: 0,
                timeout_count: 0,
                error_count: 0,
                restart_count: 0,
                last_error: None,
            }
        };
        Some(status)
    }
}

impl Engine {
    #[must_use]
    pub fn new(snapshot: EngineSnapshot) -> Self {
        let arc = Arc::new(snapshot);
        let state = EngineState::from_snapshot(arc.as_ref());
        let processor_schedule = Arc::new(ProcessorSchedule::from_snapshot(arc.as_ref()));
        Self {
            snapshot: ArcSwap::from(arc),
            state: Mutex::new(state),
            processor_schedule: ArcSwap::from(processor_schedule),
            processor_controls: ArcSwap::from_pointee(ProcessorControlSnapshot::default()),
        }
    }

    pub fn swap_snapshot(&self, snapshot: EngineSnapshot) {
        let arc = Arc::new(snapshot);
        self.processor_schedule
            .store(Arc::new(ProcessorSchedule::from_snapshot(arc.as_ref())));
        let mut state = self.state.lock();
        *state = EngineState::from_snapshot(arc.as_ref());
        self.snapshot.store(arc);
    }

    pub fn swap_processor_schedule(&self, schedule: ProcessorSchedule) {
        self.processor_schedule.store(Arc::new(schedule));
    }

    pub fn replace_processor_controls(&self, controls: ProcessorControlSnapshot) {
        self.processor_controls.store(Arc::new(controls));
    }

    pub fn update_processor_control(
        &self,
        processor_id: impl Into<String>,
        control: ProcessorControl,
    ) {
        let mut next = self.processor_controls.load().as_ref().clone();
        next.insert(processor_id.into(), control);
        self.processor_controls.store(Arc::new(next));
    }

    #[must_use]
    pub fn processor_runtime_stats(&self) -> BTreeMap<String, ProcessorRuntimeStats> {
        self.processor_schedule.load().stats()
    }

    #[must_use]
    pub fn plugin_runtime_status(&self) -> PluginHostRuntimeStatus {
        self.processor_schedule.load().plugin_runtime_status()
    }

    pub fn render_cycle(
        &self,
        frames: usize,
        sources: &HashMap<String, Vec<f32>>,
    ) -> Result<RenderOutput, EngineError> {
        let mut sinks = HashMap::<String, Vec<f32>>::new();
        let counters = self.render_cycle_with(frames, sources, |id, data| {
            sinks.insert(id.to_string(), data.to_vec());
        })?;

        Ok(RenderOutput { sinks, counters })
    }

    pub fn render_cycle_into(
        &self,
        frames: usize,
        sources: &HashMap<String, Vec<f32>>,
        sink_outputs: &mut HashMap<String, Vec<f32>>,
    ) -> Result<RuntimeCounters, EngineError> {
        self.render_cycle_with(frames, sources, |id, data| {
            let Some(target) = sink_outputs.get_mut(id) else {
                return;
            };
            if target.len() != data.len() {
                return;
            }
            target.copy_from_slice(data);
        })
    }

    pub fn render_cycle_with<F>(
        &self,
        frames: usize,
        sources: &HashMap<String, Vec<f32>>,
        mut on_sink: F,
    ) -> Result<RuntimeCounters, EngineError>
    where
        F: FnMut(&str, &[f32]),
    {
        if frames == 0 {
            return Err(EngineError::InvalidFrames);
        }

        let snapshot = self.snapshot.load();
        let graph = &snapshot.graph;
        let processor_schedule = self.processor_schedule.load();
        let processor_controls = self.processor_controls.load();
        let mut state = self.state.lock();
        prepare_node_buffers(&mut state.node_buffers, graph, frames);

        for (id, samples) in sources {
            let Some(node) = graph.nodes.get(id) else {
                continue;
            };

            if !samples.len().is_multiple_of(node.channels as usize) {
                return Err(EngineError::InvalidSourceSampleCount {
                    id: id.clone(),
                    channels: node.channels,
                });
            }

            let Some(buffer) = state.node_buffers.get_mut(id) else {
                continue;
            };
            let max = buffer.len().min(samples.len());
            buffer[..max].copy_from_slice(&samples[..max]);
        }

        let EngineState {
            routes,
            outgoing_routes,
            sink_nodes,
            sink_contributions,
            node_buffers,
            edge_scratch,
            counters: _,
        } = &mut *state;

        for contributions in sink_contributions.values_mut() {
            *contributions = 0;
        }

        let compile_order = &graph.compiled_route_plan().topological_order;
        for node_id in compile_order {
            let Some(route_indices) = outgoing_routes.get(node_id) else {
                continue;
            };

            for route_index in route_indices {
                let route_index = *route_index;
                let route = &mut routes[route_index];
                let destination_len = frames.saturating_mul(route.destination_channels);
                if edge_scratch.len() < destination_len {
                    edge_scratch.resize(destination_len, 0.0);
                }
                let scratch = &mut edge_scratch[..destination_len];
                {
                    let Some(src_buffer) = node_buffers.get(&route.from) else {
                        continue;
                    };
                    matrix_mix_into(
                        src_buffer,
                        route.source_channels,
                        route.destination_channels,
                        frames,
                        &route.matrix,
                        scratch,
                    );
                }

                processor_schedule.process_route(
                    &route.id,
                    scratch,
                    route.destination_channels,
                    frames,
                    processor_controls.as_ref(),
                );
                apply_gain(scratch, route.gain_db, route.mute);
                apply_pan(scratch, route.destination_channels, clamp_pan(route.pan));
                route.delay_line.process_in_place(scratch);

                if let Some(dst) = node_buffers.get_mut(&route.to) {
                    accumulate(dst, scratch);
                    if let Some(contributions) = sink_contributions.get_mut(&route.to) {
                        *contributions += 1;
                    }
                }
            }
        }

        for sink_id in sink_nodes {
            let Some(node) = graph.nodes.get(sink_id) else {
                continue;
            };
            let contribution_count = sink_contributions.get(sink_id).copied().unwrap_or(0);
            let Some(buffer) = node_buffers.get_mut(sink_id) else {
                continue;
            };

            if let Some(mix) = node.mix.as_ref() {
                if matches!(mix.mode, MixMode::Average) {
                    if contribution_count > 1 {
                        for sample in buffer.iter_mut() {
                            *sample /= contribution_count as f32;
                        }
                    }
                }

                if mix.limiter {
                    apply_soft_limiter(buffer, mix.limit_dbfs);
                }
            }
            on_sink(sink_id, buffer);
        }

        Ok(state.counters.clone())
    }
}

impl EngineState {
    fn from_snapshot(snapshot: &EngineSnapshot) -> Self {
        let mut routes = Vec::<RouteRuntime>::new();
        let mut outgoing_routes = HashMap::<String, Vec<usize>>::new();
        let mut max_destination_channels = 0usize;

        for route in &snapshot.graph.compiled_route_plan().routes {
            max_destination_channels =
                max_destination_channels.max(route.destination_channels as usize);
            let delay_frames = ((route.delay_ms / 1000.0) * snapshot.sample_rate as f32)
                .round()
                .max(0.0) as usize;
            let route_index = routes.len();
            routes.push(RouteRuntime {
                id: route.id.clone(),
                from: route.from.clone(),
                to: route.to.clone(),
                source_channels: route.source_channels as usize,
                destination_channels: route.destination_channels as usize,
                matrix: route.matrix.clone(),
                gain_db: route.gain_db,
                mute: route.mute,
                pan: route.pan,
                delay_line: DelayLine::new(delay_frames, route.destination_channels as usize),
            });
            outgoing_routes
                .entry(route.from.clone())
                .or_default()
                .push(route_index);
        }

        let sink_nodes = snapshot
            .graph
            .nodes
            .iter()
            .filter_map(|(id, node)| {
                if node.kind.is_sink() {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let sink_contributions = sink_nodes
            .iter()
            .cloned()
            .map(|id| (id, 0usize))
            .collect::<HashMap<_, _>>();
        let scratch_len = snapshot
            .buffer_frames
            .saturating_mul(max_destination_channels as u32) as usize;

        Self {
            routes,
            outgoing_routes,
            sink_nodes,
            sink_contributions,
            node_buffers: snapshot
                .graph
                .nodes
                .keys()
                .cloned()
                .map(|id| (id, Vec::new()))
                .collect(),
            edge_scratch: vec![0.0; scratch_len],
            counters: RuntimeCounters::default(),
        }
    }
}

fn clamp_pan(value: f32) -> f32 {
    value.clamp(-1.0, 1.0)
}

fn apply_gain(samples: &mut [f32], gain_db: f32, mute: bool) {
    if mute {
        samples.fill(0.0);
        return;
    }

    let gain = 10_f32.powf(gain_db / 20.0);
    for sample in samples {
        *sample *= gain;
    }
}

fn apply_pan(samples: &mut [f32], channels: usize, pan: f32) {
    if channels != 2 {
        return;
    }

    let theta = (pan + 1.0) * FRAC_PI_4;
    let left_gain = theta.cos();
    let right_gain = theta.sin();

    for frame in samples.chunks_exact_mut(2) {
        frame[0] *= left_gain;
        frame[1] *= right_gain;
    }
}

fn apply_soft_limiter(samples: &mut [f32], limit_dbfs: f32) {
    let limit = 10_f32.powf(limit_dbfs / 20.0).clamp(0.0001, 1.0);
    for sample in samples {
        let normalized = (*sample / limit).clamp(-6.0, 6.0);
        *sample = normalized.tanh() * limit;
    }
}

fn accumulate(target: &mut [f32], source: &[f32]) {
    let n = target.len().min(source.len());
    for (dst, src) in target[..n].iter_mut().zip(source[..n].iter()) {
        *dst += *src;
    }
}

fn matrix_mix_into(
    source: &[f32],
    source_channels: usize,
    destination_channels: usize,
    frames: usize,
    matrix: &[f32],
    out: &mut [f32],
) {
    let expected_matrix_len = source_channels.saturating_mul(destination_channels);
    if matrix.len() != expected_matrix_len {
        out.fill(0.0);
        return;
    }
    out.fill(0.0);

    for frame in 0..frames {
        let src_base = frame.saturating_mul(source_channels);
        let dst_base = frame.saturating_mul(destination_channels);

        for destination_channel in 0..destination_channels {
            let row_base = destination_channel.saturating_mul(source_channels);
            let mut mixed = 0.0f32;
            for source_channel in 0..source_channels {
                let source_sample = source
                    .get(src_base + source_channel)
                    .copied()
                    .unwrap_or(0.0);
                mixed += source_sample * matrix[row_base + source_channel];
            }
            out[dst_base + destination_channel] = mixed;
        }
    }
}

fn prepare_node_buffers(
    node_buffers: &mut HashMap<String, Vec<f32>>,
    graph: &RoutingGraph,
    frames: usize,
) {
    for (id, node) in &graph.nodes {
        let len = frames * node.channels as usize;
        let Some(buffer) = node_buffers.get_mut(id) else {
            continue;
        };
        if buffer.len() != len {
            buffer.resize(len, 0.0);
        } else {
            buffer.fill(0.0);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::f32::consts::{FRAC_1_SQRT_2, PI};
    use std::fs;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use mars_graph::build_routing_graph;
    use mars_types::{
        Bus, MixConfig, Pipe, ProcessorChain, ProcessorDefinition, ProcessorKind, Profile, Route,
        RouteMatrix, VirtualInputDevice, VirtualOutputDevice,
    };
    use serde_json::json;

    use super::{Engine, EngineSnapshot, ProcessorControl, ProcessorSchedule};

    fn test_engine() -> Engine {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "app".to_string(),
            name: "App".to_string(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: Some(2),
            uid: None,
            mix: Some(MixConfig {
                limiter: false,
                limit_dbfs: -1.0,
                mode: mars_types::MixMode::Sum,
            }),
        });
        profile.pipes.push(Pipe {
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            gain_db: -6.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        let graph = build_routing_graph(&profile).expect("graph");

        Engine::new(EngineSnapshot {
            graph,
            sample_rate: 48_000,
            buffer_frames: 256,
        })
    }

    fn stereo_identity_matrix() -> RouteMatrix {
        RouteMatrix {
            rows: 2,
            cols: 2,
            coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
        }
    }

    #[test]
    fn applies_gain() {
        let engine = test_engine();
        let mut sources = HashMap::new();
        sources.insert("app".to_string(), vec![1.0, 1.0, 1.0, 1.0]);

        let output = engine.render_cycle(2, &sources).expect("render");
        let sink = output.sinks.get("mix").expect("sink");
        assert!(sink[0] < 1.0);
        assert!(sink[0] > 0.3);
    }

    #[test]
    fn matrix_route_keeps_legacy_stereo_equivalence() {
        let mut legacy_profile = Profile::default();
        legacy_profile
            .virtual_devices
            .outputs
            .push(VirtualOutputDevice {
                id: "app".to_string(),
                name: "App".to_string(),
                channels: Some(2),
                uid: None,
                hidden: false,
            });
        legacy_profile
            .virtual_devices
            .inputs
            .push(VirtualInputDevice {
                id: "mix".to_string(),
                name: "Mix".to_string(),
                channels: Some(2),
                uid: None,
                mix: None,
            });
        legacy_profile.pipes.push(Pipe {
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            gain_db: -3.0,
            mute: false,
            pan: 0.25,
            delay_ms: 0.0,
        });

        let mut matrix_profile = Profile::default();
        matrix_profile.virtual_devices = legacy_profile.virtual_devices.clone();
        matrix_profile.routes.push(Route {
            id: "route-main".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: stereo_identity_matrix(),
            chain: None,
            gain_db: -3.0,
            mute: false,
            pan: 0.25,
            delay_ms: 0.0,
        });

        let legacy_engine = Engine::new(EngineSnapshot {
            graph: build_routing_graph(&legacy_profile).expect("legacy graph"),
            sample_rate: 48_000,
            buffer_frames: 256,
        });
        let matrix_engine = Engine::new(EngineSnapshot {
            graph: build_routing_graph(&matrix_profile).expect("matrix graph"),
            sample_rate: 48_000,
            buffer_frames: 256,
        });

        let mut sources = HashMap::new();
        let source = (0..(256 * 2))
            .map(|index| ((index as f32) * 0.003).sin())
            .collect::<Vec<_>>();
        sources.insert("app".to_string(), source);

        let legacy_output = legacy_engine
            .render_cycle(256, &sources)
            .expect("legacy render");
        let matrix_output = matrix_engine
            .render_cycle(256, &sources)
            .expect("matrix render");
        let legacy_sink = legacy_output.sinks.get("mix").expect("legacy sink");
        let matrix_sink = matrix_output.sinks.get("mix").expect("matrix sink");

        assert_eq!(legacy_sink.len(), matrix_sink.len());
        for (legacy, matrix) in legacy_sink.iter().zip(matrix_sink.iter()) {
            assert!((*legacy - *matrix).abs() < 1e-6);
        }
    }

    fn processor_chain_profile(chain_id: &str, processor_id: &str) -> Profile {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "app".to_string(),
            name: "App".to_string(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: Some(2),
            uid: None,
            mix: None,
        });
        profile.processors.push(ProcessorDefinition {
            id: processor_id.to_string(),
            kind: ProcessorKind::Eq,
            config: Default::default(),
        });
        profile.processor_chains.push(ProcessorChain {
            id: chain_id.to_string(),
            processors: vec![processor_id.to_string()],
        });
        profile.routes.push(Route {
            id: "main-route".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: stereo_identity_matrix(),
            chain: Some(chain_id.to_string()),
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile
    }

    fn single_processor_profile(
        processor_kind: ProcessorKind,
        processor_config: serde_json::Value,
        sample_rate: u32,
        buffer_frames: u32,
    ) -> Engine {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "src".to_string(),
            name: "Src".to_string(),
            channels: Some(1),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "sink".to_string(),
            name: "Sink".to_string(),
            channels: Some(1),
            uid: None,
            mix: None,
        });
        profile.processors.push(ProcessorDefinition {
            id: "proc-1".to_string(),
            kind: processor_kind,
            config: processor_config,
        });
        profile.processor_chains.push(ProcessorChain {
            id: "chain-1".to_string(),
            processors: vec!["proc-1".to_string()],
        });
        profile.routes.push(Route {
            id: "route-1".to_string(),
            from: "src".to_string(),
            to: "sink".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 1,
                cols: 1,
                coefficients: vec![vec![1.0]],
            },
            chain: Some("chain-1".to_string()),
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("graph");
        Engine::new(EngineSnapshot {
            graph,
            sample_rate,
            buffer_frames,
        })
    }

    fn unique_temp_path(tag: &str, ext: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("mars-au-{tag}-{ts}-{}.{}", std::process::id(), ext))
    }

    fn write_mock_plugin_host_script(crash_after_process: u64, gain: f32) -> PathBuf {
        let script_path = unique_temp_path("mock-plugin-host", "py");
        let script = format!(
            r#"#!/usr/bin/env python3
import json
import os
import socket
import sys

CRASH_AFTER = {crash_after_process}
GAIN = {gain}
PROCESS_COUNT = 0

def send(conn, obj):
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))

def parse_socket(argv):
    for i, arg in enumerate(argv):
        if arg == "--socket" and i + 1 < len(argv):
            return argv[i + 1]
    raise RuntimeError("missing --socket")

def main():
    socket_path = parse_socket(sys.argv[1:])
    if os.path.exists(socket_path):
        os.remove(socket_path)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(socket_path)
    listener.listen(1)
    conn, _ = listener.accept()
    instances = {{}}
    buffer = b""
    global PROCESS_COUNT
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            break
        buffer += chunk
        while b"\n" in buffer:
            line, buffer = buffer.split(b"\n", 1)
            if not line:
                continue
            request = json.loads(line.decode("utf-8"))
            kind = request["kind"]
            if kind == "handshake":
                send(conn, {{"kind": "handshake", "protocol_version": 1}})
            elif kind == "load":
                instances[request["instance_id"]] = {{"prepared": False}}
                send(conn, {{"kind": "ack"}})
            elif kind == "prepare":
                instance = instances.get(request["instance_id"])
                if instance is None:
                    send(conn, {{"kind": "error", "message": "missing instance"}})
                else:
                    instance["prepared"] = True
                    send(conn, {{"kind": "ack"}})
            elif kind == "process":
                PROCESS_COUNT += 1
                if CRASH_AFTER > 0 and PROCESS_COUNT >= CRASH_AFTER:
                    os._exit(77)
                processed = [float(sample) * GAIN for sample in request["samples"]]
                send(conn, {{"kind": "processed", "samples": processed}})
            elif kind == "reset":
                send(conn, {{"kind": "ack"}})
            elif kind == "unload":
                instances.pop(request["instance_id"], None)
                send(conn, {{"kind": "ack"}})
            elif kind == "shutdown":
                send(conn, {{"kind": "ack"}})
                conn.close()
                listener.close()
                if os.path.exists(socket_path):
                    os.remove(socket_path)
                return
            else:
                send(conn, {{"kind": "error", "message": "unknown kind"}})

if __name__ == "__main__":
    main()
"#
        );
        fs::write(&script_path, script).expect("write mock plugin host script");
        script_path
    }

    fn au_engine(processor_id: &str, script_path: &PathBuf) -> Engine {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "src".to_string(),
            name: "Src".to_string(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "sink".to_string(),
            name: "Sink".to_string(),
            channels: Some(2),
            uid: None,
            mix: None,
        });
        profile.processors.push(ProcessorDefinition {
            id: processor_id.to_string(),
            kind: ProcessorKind::Au,
            config: json!({
                "api": "auv2",
                "component_type": "aufx",
                "component_subtype": "gain",
                "component_manufacturer": "appl",
                "process_timeout_ms": 25,
                "max_frames": 2048,
                "host_command": "python3",
                "host_args": [script_path.to_string_lossy().to_string()],
            }),
        });
        profile.processor_chains.push(ProcessorChain {
            id: "chain-au".to_string(),
            processors: vec![processor_id.to_string()],
        });
        profile.routes.push(Route {
            id: "route-au".to_string(),
            from: "src".to_string(),
            to: "sink".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: Some("chain-au".to_string()),
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        let graph = build_routing_graph(&profile).expect("build graph");
        Engine::new(EngineSnapshot {
            graph,
            sample_rate: 48_000,
            buffer_frames: 256,
        })
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|item| item * item).sum::<f32>() / samples.len() as f32).sqrt()
    }

    fn biquad_reference_gain_db(
        coefficients: super::BiquadCoefficients,
        sample_rate: u32,
        freq_hz: f32,
    ) -> f32 {
        let omega = 2.0 * PI * (freq_hz / sample_rate as f32);
        let cos1 = omega.cos();
        let sin1 = omega.sin();
        let cos2 = (2.0 * omega).cos();
        let sin2 = (2.0 * omega).sin();

        let numerator_re = coefficients.b0 + coefficients.b1 * cos1 + coefficients.b2 * cos2;
        let numerator_im = -(coefficients.b1 * sin1 + coefficients.b2 * sin2);
        let denominator_re = 1.0 + coefficients.a1 * cos1 + coefficients.a2 * cos2;
        let denominator_im = -(coefficients.a1 * sin1 + coefficients.a2 * sin2);

        let numerator_mag = (numerator_re * numerator_re + numerator_im * numerator_im).sqrt();
        let denominator_mag =
            (denominator_re * denominator_re + denominator_im * denominator_im).sqrt();
        20.0 * (numerator_mag / denominator_mag).max(1e-12).log10()
    }

    #[test]
    fn eq_gain_matches_reference_vectors() {
        let sample_rate = 48_000u32;
        let frames = 256usize;
        let test_freqs = [250.0_f32, 1_000.0, 4_000.0];
        let eq_config = json!({
            "bands": [{
                "freq_hz": 1000.0,
                "q": 1.0,
                "gain_db": 6.0,
                "enabled": true
            }]
        });
        let coefficients = super::peaking_coefficients(sample_rate, 1_000.0, 1.0, 6.0);

        for freq_hz in test_freqs {
            let engine = single_processor_profile(
                ProcessorKind::Eq,
                eq_config.clone(),
                sample_rate,
                frames as u32,
            );
            let mut captured_in = Vec::new();
            let mut captured_out = Vec::new();
            let mut phase = 0.0_f32;
            let phase_step = 2.0 * PI * freq_hz / sample_rate as f32;

            for cycle in 0..100 {
                let mut input = Vec::with_capacity(frames);
                for _ in 0..frames {
                    input.push(phase.sin());
                    phase += phase_step;
                    if phase > 2.0 * PI {
                        phase -= 2.0 * PI;
                    }
                }
                let mut sources = HashMap::new();
                sources.insert("src".to_string(), input.clone());
                let output = engine.render_cycle(frames, &sources).expect("render");
                if cycle >= 40 {
                    captured_in.extend(input);
                    captured_out.extend_from_slice(output.sinks.get("sink").expect("sink"));
                }
            }

            let measured_gain_db =
                20.0 * (rms(&captured_out) / rms(&captured_in)).max(1e-12).log10();
            let expected_gain_db = biquad_reference_gain_db(coefficients, sample_rate, freq_hz);
            assert!(
                (measured_gain_db - expected_gain_db).abs() < 0.75,
                "freq={freq_hz}Hz measured={measured_gain_db:.3}dB expected={expected_gain_db:.3}dB"
            );
        }
    }

    #[test]
    fn dynamics_threshold_and_ratio_match_expected_reduction() {
        let sample_rate = 48_000u32;
        let frames = 256usize;
        let engine = single_processor_profile(
            ProcessorKind::Dynamics,
            json!({
                "threshold_db": -12.0,
                "ratio": 4.0,
                "attack_ms": 0.1,
                "release_ms": 250.0,
                "makeup_gain_db": 0.0,
                "limiter": false
            }),
            sample_rate,
            frames as u32,
        );

        let mut sources = HashMap::new();
        sources.insert("src".to_string(), vec![1.0_f32; frames]);
        let mut captured = Vec::new();
        for cycle in 0..80 {
            let output = engine.render_cycle(frames, &sources).expect("render");
            if cycle >= 40 {
                captured.extend_from_slice(output.sinks.get("sink").expect("sink"));
            }
        }

        let observed = captured.iter().map(|item| item.abs()).sum::<f32>() / captured.len() as f32;
        let expected_gain = 10.0_f32.powf(-9.0 / 20.0);
        assert!((observed - expected_gain).abs() < 0.04);
    }

    #[test]
    fn dynamics_attack_and_release_have_expected_temporal_shape() {
        let sample_rate = 48_000u32;
        let frames = 256usize;
        let engine = single_processor_profile(
            ProcessorKind::Dynamics,
            json!({
                "threshold_db": -18.0,
                "ratio": 8.0,
                "attack_ms": 2.0,
                "release_ms": 120.0,
                "makeup_gain_db": 0.0,
                "limiter": false
            }),
            sample_rate,
            frames as u32,
        );

        let mut silence = HashMap::new();
        silence.insert("src".to_string(), vec![0.0_f32; frames]);
        engine.render_cycle(frames, &silence).expect("render");

        let mut hot = HashMap::new();
        hot.insert("src".to_string(), vec![1.0_f32; frames]);
        let attack_output = engine.render_cycle(frames, &hot).expect("render");
        let attack_sink = attack_output.sinks.get("sink").expect("sink");
        let early_mean = attack_sink[..32].iter().map(|item| item.abs()).sum::<f32>() / 32.0;
        let late_mean = attack_sink[(frames - 32)..]
            .iter()
            .map(|item| item.abs())
            .sum::<f32>()
            / 32.0;
        assert!(early_mean > late_mean + 0.03);

        for _ in 0..40 {
            engine.render_cycle(frames, &hot).expect("render");
        }

        let mut cool = HashMap::new();
        cool.insert("src".to_string(), vec![0.1_f32; frames]);
        let release_early = engine.render_cycle(frames, &cool).expect("render");
        let early_mean = release_early
            .sinks
            .get("sink")
            .expect("sink")
            .iter()
            .map(|item| item.abs())
            .sum::<f32>()
            / frames as f32;

        for _ in 0..140 {
            engine.render_cycle(frames, &cool).expect("render");
        }
        let release_late = engine.render_cycle(frames, &cool).expect("render");
        let late_mean = release_late
            .sinks
            .get("sink")
            .expect("sink")
            .iter()
            .map(|item| item.abs())
            .sum::<f32>()
            / frames as f32;

        assert!(late_mean > early_mean + 0.01);
        assert!((late_mean - 0.1).abs() < 0.01);
    }

    #[test]
    fn denoise_reduces_low_level_noise_and_preserves_primary_signal() {
        let sample_rate = 48_000u32;
        let frames = 256usize;
        let engine = single_processor_profile(
            ProcessorKind::Denoise,
            json!({
                "threshold_db": -30.0,
                "reduction_db": 24.0,
                "attack_ms": 2.0,
                "release_ms": 120.0
            }),
            sample_rate,
            frames as u32,
        );

        let low_amp = 0.005_f32;
        let mut low_sources = HashMap::new();
        low_sources.insert("src".to_string(), vec![low_amp; frames]);
        let mut low_capture = Vec::new();
        for cycle in 0..100 {
            let output = engine.render_cycle(frames, &low_sources).expect("render");
            if cycle >= 40 {
                low_capture.extend_from_slice(output.sinks.get("sink").expect("sink"));
            }
        }
        let low_out_rms = rms(&low_capture);

        let high_amp = 0.2_f32;
        let mut high_sources = HashMap::new();
        high_sources.insert("src".to_string(), vec![high_amp; frames]);
        let mut high_capture = Vec::new();
        for cycle in 0..60 {
            let output = engine.render_cycle(frames, &high_sources).expect("render");
            if cycle >= 20 {
                high_capture.extend_from_slice(output.sinks.get("sink").expect("sink"));
            }
        }
        let high_out_rms = rms(&high_capture);

        assert!(low_out_rms <= low_amp * 0.15);
        assert!(high_out_rms >= high_amp * 0.85);
    }

    #[test]
    fn time_shift_matches_configured_latency() {
        let sample_rate = 1_000u32;
        let frames = 20usize;
        let delay_frames = 60usize;
        let engine = single_processor_profile(
            ProcessorKind::TimeShift,
            json!({
                "delay_ms": delay_frames as f32,
                "max_delay_ms": 200.0
            }),
            sample_rate,
            frames as u32,
        );

        let mut captured = Vec::new();
        for cycle in 0..6 {
            let mut sources = HashMap::new();
            let mut input = vec![0.0_f32; frames];
            if cycle == 0 {
                input[0] = 1.0;
            }
            sources.insert("src".to_string(), input);
            let output = engine.render_cycle(frames, &sources).expect("render");
            captured.extend_from_slice(output.sinks.get("sink").expect("sink"));
        }

        assert!(
            captured
                .iter()
                .take(delay_frames)
                .all(|sample| sample.abs() < 1e-9)
        );
        assert!((captured[delay_frames] - 1.0).abs() < 1e-6);
        assert!(
            captured
                .iter()
                .enumerate()
                .all(|(index, sample)| index == delay_frames || sample.abs() < 1e-5)
        );
    }

    #[test]
    fn time_shift_state_resets_on_snapshot_swap() {
        let sample_rate = 1_000u32;
        let frames = 10usize;
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "src".to_string(),
            name: "Src".to_string(),
            channels: Some(1),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "sink".to_string(),
            name: "Sink".to_string(),
            channels: Some(1),
            uid: None,
            mix: None,
        });
        profile.processors.push(ProcessorDefinition {
            id: "ts".to_string(),
            kind: ProcessorKind::TimeShift,
            config: json!({
                "delay_ms": 20.0,
                "max_delay_ms": 200.0
            }),
        });
        profile.processor_chains.push(ProcessorChain {
            id: "chain".to_string(),
            processors: vec!["ts".to_string()],
        });
        profile.routes.push(Route {
            id: "route".to_string(),
            from: "src".to_string(),
            to: "sink".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 1,
                cols: 1,
                coefficients: vec![vec![1.0]],
            },
            chain: Some("chain".to_string()),
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        let graph = build_routing_graph(&profile).expect("graph");
        let snapshot = EngineSnapshot {
            graph,
            sample_rate,
            buffer_frames: frames as u32,
        };
        let engine = Engine::new(snapshot.clone());

        let mut cycle1 = HashMap::new();
        let mut impulse = vec![0.0_f32; frames];
        impulse[0] = 1.0;
        cycle1.insert("src".to_string(), impulse);
        let out1 = engine.render_cycle(frames, &cycle1).expect("cycle1");
        assert!(out1.sinks["sink"].iter().all(|sample| sample.abs() < 1e-9));

        let mut silence = HashMap::new();
        silence.insert("src".to_string(), vec![0.0_f32; frames]);
        let out2 = engine.render_cycle(frames, &silence).expect("cycle2");
        assert!(out2.sinks["sink"].iter().all(|sample| sample.abs() < 1e-9));

        engine.swap_snapshot(snapshot);
        let out3 = engine.render_cycle(frames, &silence).expect("cycle3");
        let out4 = engine.render_cycle(frames, &silence).expect("cycle4");
        assert!(out3.sinks["sink"].iter().all(|sample| sample.abs() < 1e-9));
        assert!(out4.sinks["sink"].iter().all(|sample| sample.abs() < 1e-9));
    }

    #[test]
    fn au_processor_applies_processed_samples_from_host() {
        let script_path = write_mock_plugin_host_script(0, 0.5);
        let engine = au_engine("au-main", &script_path);

        let mut sources = HashMap::new();
        sources.insert("src".to_string(), vec![0.8; 256 * 2]);
        let mut observed = None;
        for _ in 0..64 {
            let output = engine.render_cycle(256, &sources).expect("render");
            let sink = output.sinks.get("sink").expect("sink");
            let mean = sink.iter().copied().sum::<f32>() / sink.len() as f32;
            if mean < 0.7 {
                observed = Some(mean);
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        let mean = observed.expect("plugin host did not return processed frames in time");
        assert!(
            mean > 0.3,
            "processed mean should stay above silence floor, got {mean}"
        );

        let _ = fs::remove_file(script_path);
    }

    #[test]
    fn au_processor_reload_rebinds_new_instance() {
        let script_path = write_mock_plugin_host_script(0, 1.0);
        let engine = au_engine("au-a", &script_path);

        let mut sources = HashMap::new();
        sources.insert("src".to_string(), vec![0.25; 256 * 2]);
        for _ in 0..16 {
            engine.render_cycle(256, &sources).expect("render");
            thread::sleep(Duration::from_millis(2));
        }
        let before = engine.plugin_runtime_status();
        assert_eq!(before.instances.len(), 1);

        let replacement = au_engine("au-b", &script_path);
        engine.swap_snapshot(EngineSnapshot {
            graph: replacement.snapshot.load().graph.clone(),
            sample_rate: 48_000,
            buffer_frames: 256,
        });

        for _ in 0..16 {
            engine.render_cycle(256, &sources).expect("render");
            thread::sleep(Duration::from_millis(2));
        }

        let after = engine.plugin_runtime_status();
        assert_eq!(after.instances.len(), 1);
        assert_eq!(after.instances[0].id, "au-b");
        assert!(after.instances[0].process_calls > 0);

        let _ = fs::remove_file(script_path);
    }

    #[test]
    fn au_processor_recovers_after_host_crash() {
        let script_path = write_mock_plugin_host_script(1, 1.0);
        let engine = au_engine("au-crash", &script_path);

        let mut sources = HashMap::new();
        sources.insert("src".to_string(), vec![0.1; 256 * 2]);
        let mut observed_fault = false;
        for _ in 0..80 {
            engine.render_cycle(256, &sources).expect("render");
            thread::sleep(Duration::from_millis(5));
            let runtime = engine.plugin_runtime_status();
            if runtime.restart_count > 0 || runtime.error_count > 0 {
                observed_fault = true;
                break;
            }
        }

        let status = engine.plugin_runtime_status();
        assert_eq!(status.instances.len(), 1);
        assert!(
            observed_fault,
            "expected plugin host crash to produce restart or error counters: {status:?}"
        );
        assert!(
            status.instances[0].process_calls > 0,
            "expected AU processor to continue processing after crash recovery"
        );

        let _ = fs::remove_file(script_path);
    }

    #[test]
    fn processor_chain_executes_and_supports_bypass_control() {
        let profile = processor_chain_profile("voice", "proc-a");
        let graph = build_routing_graph(&profile).expect("graph");
        let engine = Engine::new(EngineSnapshot {
            graph,
            sample_rate: 48_000,
            buffer_frames: 256,
        });

        let mut sources = HashMap::new();
        sources.insert("app".to_string(), vec![0.25; 256 * 2]);

        for _ in 0..8 {
            engine.render_cycle(256, &sources).expect("render");
        }
        let before = engine
            .processor_runtime_stats()
            .get("proc-a")
            .expect("processor stats")
            .process_calls;
        assert!(before > 0);

        engine.update_processor_control(
            "proc-a",
            ProcessorControl {
                bypass: true,
                generation: 3,
                params: BTreeMap::new(),
            },
        );
        for _ in 0..8 {
            engine.render_cycle(256, &sources).expect("render");
        }
        let after = engine
            .processor_runtime_stats()
            .get("proc-a")
            .expect("processor stats")
            .process_calls;
        assert_eq!(before, after);
    }

    #[test]
    fn processor_schedule_hot_swap_rebinds_active_chain() {
        let profile_a = processor_chain_profile("voice-a", "proc-a");
        let graph_a = build_routing_graph(&profile_a).expect("graph a");
        let profile_b = processor_chain_profile("voice-b", "proc-b");
        let graph_b = build_routing_graph(&profile_b).expect("graph b");

        let engine = Engine::new(EngineSnapshot {
            graph: graph_a.clone(),
            sample_rate: 48_000,
            buffer_frames: 256,
        });
        let mut sources = HashMap::new();
        sources.insert("app".to_string(), vec![0.2; 256 * 2]);

        for _ in 0..4 {
            engine.render_cycle(256, &sources).expect("render");
        }
        let before = engine.processor_runtime_stats();
        assert!(
            before
                .get("proc-a")
                .map(|item| item.process_calls)
                .unwrap_or(0)
                > 0
        );

        let swapped_snapshot = EngineSnapshot {
            graph: graph_b.clone(),
            sample_rate: 48_000,
            buffer_frames: 256,
        };
        engine.swap_processor_schedule(ProcessorSchedule::from_snapshot(&swapped_snapshot));
        for _ in 0..4 {
            engine.render_cycle(256, &sources).expect("render");
        }

        let after = engine.processor_runtime_stats();
        assert!(
            after
                .get("proc-b")
                .map(|item| item.process_calls)
                .unwrap_or(0)
                > 0
        );
        assert_eq!(
            after
                .get("proc-a")
                .map(|item| item.process_calls)
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn delay_outputs_silence_first_cycle() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "app".to_string(),
            name: "App".to_string(),
            channels: Some(1),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: Some(1),
            uid: None,
            mix: None,
        });
        profile.pipes.push(Pipe {
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 10.0,
        });
        let graph = build_routing_graph(&profile).expect("graph");
        let engine = Engine::new(EngineSnapshot {
            graph,
            sample_rate: 100,
            buffer_frames: 1,
        });

        let mut sources = HashMap::new();
        sources.insert("app".to_string(), vec![1.0]);
        let output = engine.render_cycle(1, &sources).expect("render");
        assert_eq!(output.sinks["mix"][0], 0.0);
    }

    #[test]
    fn sine_gain_matches_expected_rms_and_peak() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "osc".to_string(),
            name: "Osc".to_string(),
            channels: Some(1),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: Some(1),
            uid: None,
            mix: None,
        });
        profile.pipes.push(Pipe {
            from: "osc".to_string(),
            to: "mix".to_string(),
            enabled: true,
            gain_db: -6.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("graph");
        let engine = Engine::new(EngineSnapshot {
            graph,
            sample_rate: 48_000,
            buffer_frames: 256,
        });

        let frames = 256usize;
        let freq_hz = 1_000.0f32;
        let sample_rate = 48_000.0f32;
        let mut sine = vec![0.0_f32; frames];
        for (index, sample) in sine.iter_mut().enumerate() {
            let phase = 2.0 * PI * freq_hz * (index as f32 / sample_rate);
            *sample = phase.sin();
        }

        let mut sources = HashMap::new();
        sources.insert("osc".to_string(), sine);
        let output = engine.render_cycle(frames, &sources).expect("render");
        let sink = output.sinks.get("mix").expect("sink");

        let rms =
            (sink.iter().map(|sample| sample * sample).sum::<f32>() / sink.len() as f32).sqrt();
        let peak = sink
            .iter()
            .fold(0.0_f32, |acc, sample| acc.max(sample.abs()));

        // input RMS for full-scale sine is ~0.7071; after -6 dB it is ~0.354.
        assert!((rms - 0.354).abs() < 0.03);
        // input peak is 1.0; after -6 dB it is ~0.501.
        assert!((peak - 0.501).abs() < 0.03);
    }

    #[test]
    fn limiter_caps_peak_to_configured_dbfs() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "loud".to_string(),
            name: "Loud".to_string(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: Some(2),
            uid: None,
            mix: Some(MixConfig {
                limiter: true,
                limit_dbfs: -1.0,
                mode: mars_types::MixMode::Sum,
            }),
        });
        profile.pipes.push(Pipe {
            from: "loud".to_string(),
            to: "mix".to_string(),
            enabled: true,
            gain_db: 12.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("graph");
        let engine = Engine::new(EngineSnapshot {
            graph,
            sample_rate: 48_000,
            buffer_frames: 8,
        });

        let mut sources = HashMap::new();
        sources.insert("loud".to_string(), vec![1.0_f32; 16]);
        let output = engine.render_cycle(8, &sources).expect("render");
        let sink = output.sinks.get("mix").expect("sink");
        let peak = sink
            .iter()
            .fold(0.0_f32, |acc, sample| acc.max(sample.abs()));

        // -1 dBFS ~= 0.891.
        assert!(peak <= 0.92);
    }

    #[test]
    fn near_e2e_multisource_multioutput_mixes_as_expected() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "music".to_string(),
            name: "Music".to_string(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "voice".to_string(),
            name: "Voice".to_string(),
            channels: Some(1),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "fx".to_string(),
            name: "FX".to_string(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
        profile.buses.push(Bus {
            id: "main".to_string(),
            channels: Some(2),
            mix: None,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "stream".to_string(),
            name: "Stream".to_string(),
            channels: Some(2),
            uid: None,
            mix: Some(MixConfig {
                limiter: false,
                limit_dbfs: -1.0,
                mode: mars_types::MixMode::Sum,
            }),
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "monitor".to_string(),
            name: "Monitor".to_string(),
            channels: Some(2),
            uid: None,
            mix: Some(MixConfig {
                limiter: false,
                limit_dbfs: -1.0,
                mode: mars_types::MixMode::Average,
            }),
        });

        profile.pipes.push(Pipe {
            from: "music".to_string(),
            to: "main".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile.pipes.push(Pipe {
            from: "voice".to_string(),
            to: "main".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile.pipes.push(Pipe {
            from: "fx".to_string(),
            to: "main".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile.pipes.push(Pipe {
            from: "main".to_string(),
            to: "stream".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile.pipes.push(Pipe {
            from: "main".to_string(),
            to: "monitor".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile.pipes.push(Pipe {
            from: "music".to_string(),
            to: "monitor".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("graph");
        let engine = Engine::new(EngineSnapshot {
            graph,
            sample_rate: 48_000,
            buffer_frames: 4,
        });

        let mut sources = HashMap::new();
        sources.insert("music".to_string(), vec![1.0_f32; 8]);
        sources.insert("voice".to_string(), vec![0.5_f32; 4]);
        sources.insert("fx".to_string(), vec![0.25_f32; 8]);

        let output = engine.render_cycle(4, &sources).expect("render");
        let stream = output.sinks.get("stream").expect("stream sink");
        let monitor = output.sinks.get("monitor").expect("monitor sink");

        assert_eq!(stream.len(), 8);
        assert_eq!(monitor.len(), 8);

        let expected_stream = 1.75 * FRAC_1_SQRT_2 * FRAC_1_SQRT_2;
        let expected_monitor = (expected_stream + (1.0 * FRAC_1_SQRT_2)) / 2.0;
        for sample in stream {
            assert!((*sample - expected_stream).abs() < 1e-5);
        }
        for sample in monitor {
            assert!((*sample - expected_monitor).abs() < 1e-5);
        }
    }

    #[test]
    fn near_e2e_delay_path_keeps_state_across_cycles() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "pulse".to_string(),
            name: "Pulse".to_string(),
            channels: Some(1),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "bed".to_string(),
            name: "Bed".to_string(),
            channels: Some(1),
            uid: None,
            hidden: false,
        });
        profile.buses.push(Bus {
            id: "mix".to_string(),
            channels: Some(1),
            mix: None,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "out".to_string(),
            name: "Out".to_string(),
            channels: Some(1),
            uid: None,
            mix: None,
        });

        profile.pipes.push(Pipe {
            from: "pulse".to_string(),
            to: "mix".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 20.0,
        });
        profile.pipes.push(Pipe {
            from: "bed".to_string(),
            to: "mix".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile.pipes.push(Pipe {
            from: "mix".to_string(),
            to: "out".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("graph");
        let engine = Engine::new(EngineSnapshot {
            graph,
            sample_rate: 100,
            buffer_frames: 1,
        });

        let mut cycle1 = HashMap::new();
        cycle1.insert("pulse".to_string(), vec![1.0_f32]);
        cycle1.insert("bed".to_string(), vec![0.2_f32]);
        let out1 = engine.render_cycle(1, &cycle1).expect("cycle1");

        let mut cycle2 = HashMap::new();
        cycle2.insert("pulse".to_string(), vec![0.0_f32]);
        cycle2.insert("bed".to_string(), vec![0.2_f32]);
        let out2 = engine.render_cycle(1, &cycle2).expect("cycle2");

        let mut cycle3 = HashMap::new();
        cycle3.insert("pulse".to_string(), vec![0.0_f32]);
        cycle3.insert("bed".to_string(), vec![0.2_f32]);
        let out3 = engine.render_cycle(1, &cycle3).expect("cycle3");

        assert!((out1.sinks["out"][0] - 0.2).abs() < 1e-6);
        assert!((out2.sinks["out"][0] - 0.2).abs() < 1e-6);
        assert!((out3.sinks["out"][0] - 1.2).abs() < 1e-6);
    }
}

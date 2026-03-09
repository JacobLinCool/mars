#![forbid(unsafe_code)]
//! Realtime-safe-ish audio graph rendering engine.

use std::collections::{BTreeMap, HashMap};
use std::f32::consts::FRAC_PI_4;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arc_swap::ArcSwap;
use mars_graph::RoutingGraph;
use mars_types::{MixMode, ProcessorKind, RuntimeCounters};
use parking_lot::Mutex;
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessorRuntimeStats {
    pub prepare_calls: u64,
    pub process_calls: u64,
    pub reset_calls: u64,
    pub last_generation: u64,
}

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
struct PassthroughProcessorBlock {
    id: String,
    kind: ProcessorKind,
    prepared: AtomicBool,
    counters: ProcessorCounters,
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
                let processor: Arc<dyn ProcessorBlock> = Arc::new(PassthroughProcessorBlock::new(
                    compiled.id.clone(),
                    compiled.kind,
                ));
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

impl PassthroughProcessorBlock {
    fn new(id: String, kind: ProcessorKind) -> Self {
        Self {
            id,
            kind,
            prepared: AtomicBool::new(false),
            counters: ProcessorCounters::default(),
        }
    }
}

impl ProcessorBlock for PassthroughProcessorBlock {
    fn id(&self) -> &str {
        &self.id
    }

    fn prepare(&self, context: ProcessorPrepareContext) {
        let _ = (
            context.channels,
            context.sample_rate,
            context.max_frames,
            self.kind,
        );
        self.prepared.store(true, Ordering::Relaxed);
        self.counters.prepare_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn process(
        &self,
        _samples: &mut [f32],
        _channels: usize,
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
        self.counters.process_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        if self.prepared.swap(false, Ordering::Relaxed) {
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

    use mars_graph::build_routing_graph;
    use mars_types::{
        Bus, MixConfig, Pipe, ProcessorChain, ProcessorDefinition, ProcessorKind, Profile, Route,
        RouteMatrix, VirtualInputDevice, VirtualOutputDevice,
    };

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

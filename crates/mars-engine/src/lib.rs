#![forbid(unsafe_code)]
//! Realtime-safe-ish audio graph rendering engine.

use std::collections::HashMap;
use std::f32::consts::FRAC_PI_4;
use std::sync::Arc;

use arc_swap::ArcSwap;
use mars_graph::RoutingGraph;
use mars_types::{MixMode, RuntimeCounters};
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
}

#[derive(Debug, Default)]
struct EngineState {
    delay_lines: HashMap<String, DelayLine>,
    node_buffers: HashMap<String, Vec<f32>>,
    edge_scratch: Vec<f32>,
    counters: RuntimeCounters,
}

#[derive(Debug, Clone)]
struct DelayLine {
    data: Vec<f32>,
    write_idx: usize,
    delay_frames: usize,
    channels: usize,
}

impl DelayLine {
    fn new(delay_frames: usize, channels: usize) -> Self {
        let len = delay_frames.saturating_mul(channels);
        Self {
            data: vec![0.0; len],
            write_idx: 0,
            delay_frames,
            channels,
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

impl Engine {
    #[must_use]
    pub fn new(snapshot: EngineSnapshot) -> Self {
        let arc = Arc::new(snapshot);
        let state = EngineState::from_snapshot(arc.as_ref());
        Self {
            snapshot: ArcSwap::from(arc),
            state: Mutex::new(state),
        }
    }

    pub fn swap_snapshot(&self, snapshot: EngineSnapshot) {
        let arc = Arc::new(snapshot);
        let mut state = self.state.lock();
        *state = EngineState::from_snapshot(arc.as_ref());
        self.snapshot.store(arc);
    }

    pub fn render_cycle(
        &self,
        frames: usize,
        sources: &HashMap<String, Vec<f32>>,
    ) -> Result<RenderOutput, EngineError> {
        if frames == 0 {
            return Err(EngineError::InvalidFrames);
        }

        let snapshot = self.snapshot.load();
        let graph = &snapshot.graph;
        let mut state = self.state.lock();
        let mut edge_scratch = std::mem::take(&mut state.edge_scratch);
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

        let mut sink_contributions = HashMap::<String, usize>::new();
        for node_id in graph.topological_order() {
            let Some(source_node) = graph.nodes.get(&node_id) else {
                continue;
            };

            for edge in graph.outgoing(&node_id) {
                let Some(destination_node) = graph.nodes.get(&edge.to) else {
                    continue;
                };
                let source_channels = source_node.channels as usize;
                let dest_channels = destination_node.channels as usize;
                {
                    let Some(src_buffer) = state.node_buffers.get(&edge.from) else {
                        continue;
                    };
                    convert_channels_into(
                        src_buffer,
                        source_channels,
                        dest_channels,
                        frames,
                        &mut edge_scratch,
                    );
                }

                apply_gain(&mut edge_scratch, edge.gain_db, edge.mute);
                apply_pan(&mut edge_scratch, dest_channels, clamp_pan(edge.pan));

                let delay_frames = ((edge.delay_ms / 1000.0) * snapshot.sample_rate as f32)
                    .round()
                    .max(0.0) as usize;
                if delay_frames > 0 {
                    let line = state
                        .delay_lines
                        .entry(edge.id.clone())
                        .or_insert_with(|| DelayLine::new(delay_frames, dest_channels));
                    if line.delay_frames != delay_frames || line.channels != dest_channels {
                        *line = DelayLine::new(delay_frames, dest_channels);
                    }
                    line.process_in_place(&mut edge_scratch);
                }

                if let Some(dst) = state.node_buffers.get_mut(&edge.to) {
                    accumulate(dst, &edge_scratch);
                    *sink_contributions.entry(edge.to.clone()).or_insert(0) += 1;
                }
            }
        }

        let mut sinks = HashMap::<String, Vec<f32>>::new();
        for (id, node) in &graph.nodes {
            if !node.kind.is_sink() {
                continue;
            }

            let Some(buffer) = state.node_buffers.get(id) else {
                continue;
            };
            let mut rendered = buffer.clone();

            if let Some(mix) = node.mix.as_ref() {
                if matches!(mix.mode, MixMode::Average) {
                    let count = sink_contributions.get(id).copied().unwrap_or(0);
                    if count > 1 {
                        for sample in &mut rendered {
                            *sample /= count as f32;
                        }
                    }
                }

                if mix.limiter {
                    apply_soft_limiter(&mut rendered, mix.limit_dbfs);
                }
            }

            sinks.insert(id.clone(), rendered);
        }
        state.edge_scratch = edge_scratch;

        Ok(RenderOutput {
            sinks,
            counters: state.counters.clone(),
        })
    }
}

impl EngineState {
    fn from_snapshot(snapshot: &EngineSnapshot) -> Self {
        let mut delay_lines = HashMap::new();
        for edge in &snapshot.graph.edges {
            let Some(node) = snapshot.graph.nodes.get(&edge.to) else {
                continue;
            };

            let delay_frames = ((edge.delay_ms / 1000.0) * snapshot.sample_rate as f32)
                .round()
                .max(0.0) as usize;
            delay_lines.insert(
                edge.id.clone(),
                DelayLine::new(delay_frames, node.channels as usize),
            );
        }

        Self {
            delay_lines,
            node_buffers: snapshot
                .graph
                .nodes
                .keys()
                .cloned()
                .map(|id| (id, Vec::new()))
                .collect(),
            edge_scratch: Vec::new(),
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

fn convert_channels_into(
    source: &[f32],
    source_channels: usize,
    dest_channels: usize,
    frames: usize,
    out: &mut Vec<f32>,
) {
    out.clear();
    if source_channels == dest_channels {
        let len = source.len().min(frames * dest_channels);
        out.extend_from_slice(&source[..len]);
        if len < frames * dest_channels {
            out.resize(frames * dest_channels, 0.0);
        }
        return;
    }

    match (source_channels, dest_channels) {
        (1, 2) => {
            out.resize(frames * 2, 0.0);
            for frame in 0..frames {
                let sample = source.get(frame).copied().unwrap_or(0.0);
                out[frame * 2] = sample;
                out[frame * 2 + 1] = sample;
            }
        }
        (2, 1) => {
            out.resize(frames, 0.0);
            for (frame, sample) in out.iter_mut().enumerate().take(frames) {
                let left = source.get(frame * 2).copied().unwrap_or(0.0);
                let right = source.get(frame * 2 + 1).copied().unwrap_or(0.0);
                *sample = (left + right) * 0.5;
            }
        }
        _ => out.resize(frames * dest_channels, 0.0),
    }
}

fn prepare_node_buffers(
    node_buffers: &mut HashMap<String, Vec<f32>>,
    graph: &RoutingGraph,
    frames: usize,
) {
    node_buffers.retain(|id, _| graph.nodes.contains_key(id));
    for (id, node) in &graph.nodes {
        let len = frames * node.channels as usize;
        let buffer = node_buffers.entry(id.clone()).or_default();
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
    use std::collections::HashMap;
    use std::f32::consts::{FRAC_1_SQRT_2, PI};

    use mars_graph::build_routing_graph;
    use mars_types::{Bus, MixConfig, Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

    use super::{Engine, EngineSnapshot};

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

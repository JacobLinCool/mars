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

        let mut node_buffers: HashMap<String, Vec<f32>> = graph
            .nodes
            .iter()
            .map(|(id, node)| (id.clone(), vec![0.0; frames * node.channels as usize]))
            .collect();

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

            let Some(buffer) = node_buffers.get_mut(id) else {
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
                let (Some(src_buffer), Some(destination_node)) =
                    (node_buffers.get(&edge.from), graph.nodes.get(&edge.to))
                else {
                    continue;
                };

                let mut processed = convert_channels(
                    src_buffer,
                    source_node.channels as usize,
                    destination_node.channels as usize,
                    frames,
                );

                apply_gain(&mut processed, edge.gain_db, edge.mute);
                apply_pan(
                    &mut processed,
                    destination_node.channels as usize,
                    clamp_pan(edge.pan),
                );

                let delay_frames = ((edge.delay_ms / 1000.0) * snapshot.sample_rate as f32)
                    .round()
                    .max(0.0) as usize;
                if delay_frames > 0 {
                    let line = state.delay_lines.entry(edge.id.clone()).or_insert_with(|| {
                        DelayLine::new(delay_frames, destination_node.channels as usize)
                    });
                    if line.delay_frames != delay_frames
                        || line.channels != destination_node.channels as usize
                    {
                        *line = DelayLine::new(delay_frames, destination_node.channels as usize);
                    }
                    line.process_in_place(&mut processed);
                }

                if let Some(dst) = node_buffers.get_mut(&edge.to) {
                    accumulate(dst, &processed);
                    *sink_contributions.entry(edge.to.clone()).or_insert(0) += 1;
                }
            }
        }

        let mut sinks = HashMap::<String, Vec<f32>>::new();
        for (id, node) in &graph.nodes {
            if !node.kind.is_sink() {
                continue;
            }

            let Some(buffer) = node_buffers.remove(id) else {
                continue;
            };
            let mut rendered = buffer;

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

fn convert_channels(
    source: &[f32],
    source_channels: usize,
    dest_channels: usize,
    frames: usize,
) -> Vec<f32> {
    if source_channels == dest_channels {
        return source[..source.len().min(frames * dest_channels)].to_vec();
    }

    match (source_channels, dest_channels) {
        (1, 2) => {
            let mut out = vec![0.0; frames * 2];
            for frame in 0..frames {
                let sample = source.get(frame).copied().unwrap_or(0.0);
                out[frame * 2] = sample;
                out[frame * 2 + 1] = sample;
            }
            out
        }
        (2, 1) => {
            let mut out = vec![0.0; frames];
            for frame in 0..frames {
                let left = source.get(frame * 2).copied().unwrap_or(0.0);
                let right = source.get(frame * 2 + 1).copied().unwrap_or(0.0);
                out[frame] = (left + right) * 0.5;
            }
            out
        }
        _ => vec![0.0; frames * dest_channels],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::f32::consts::PI;

    use mars_graph::build_routing_graph;
    use mars_types::{MixConfig, Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

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
}

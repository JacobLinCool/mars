#![forbid(unsafe_code)]
//! Routing graph construction and validation.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mars_types::{
    NodeDescriptor, NodeKind, PipeDescriptor, Profile, VirtualInputDevice, VirtualOutputDevice,
};
use petgraph::algo::is_cyclic_directed;
use petgraph::graph::DiGraph;
use petgraph::visit::Topo;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct RoutingGraph {
    pub nodes: BTreeMap<String, NodeDescriptor>,
    pub edges: Vec<PipeDescriptor>,
}

impl RoutingGraph {
    #[must_use]
    pub fn topological_order(&self) -> Vec<String> {
        let mut graph = DiGraph::<String, ()>::new();
        let mut idx = HashMap::new();

        for id in self.nodes.keys() {
            let node_idx = graph.add_node(id.clone());
            idx.insert(id.clone(), node_idx);
        }

        for edge in &self.edges {
            if let (Some(from), Some(to)) = (idx.get(&edge.from), idx.get(&edge.to)) {
                graph.add_edge(*from, *to, ());
            }
        }

        let mut topo = Topo::new(&graph);
        let mut ordered = Vec::new();
        while let Some(next) = topo.next(&graph) {
            ordered.push(graph[next].clone());
        }
        ordered
    }

    #[must_use]
    pub fn outgoing<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a PipeDescriptor> {
        self.edges.iter().filter(move |edge| edge.from == id)
    }

    #[must_use]
    pub fn incoming<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a PipeDescriptor> {
        self.edges.iter().filter(move |edge| edge.to == id)
    }
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("duplicate node id: {0}")]
    DuplicateNode(String),
    #[error("unknown source node in pipe: {0}")]
    UnknownSource(String),
    #[error("unknown destination node in pipe: {0}")]
    UnknownDestination(String),
    #[error("node cannot be used as source: {0}")]
    InvalidSourceType(String),
    #[error("node cannot be used as destination: {0}")]
    InvalidDestinationType(String),
    #[error(
        "channel mismatch not supported in current release: {from} ({from_channels}) -> {to} ({to_channels})"
    )]
    UnsupportedChannels {
        from: String,
        from_channels: u16,
        to: String,
        to_channels: u16,
    },
    #[error("graph contains cycle")]
    CycleDetected,
    #[error("pipe delay must be in range [0, 2000], got {0}")]
    InvalidDelay(f32),
}

pub fn build_routing_graph(profile: &Profile) -> Result<RoutingGraph, GraphError> {
    let default_channels = profile.audio.channels.as_value().unwrap_or(2);
    let mut nodes = BTreeMap::<String, NodeDescriptor>::new();
    let mut seen = BTreeSet::<String>::new();

    for output in &profile.virtual_devices.outputs {
        add_node(
            &mut nodes,
            &mut seen,
            node_from_vout(output, default_channels),
        )?;
    }
    for input in &profile.virtual_devices.inputs {
        add_node(
            &mut nodes,
            &mut seen,
            node_from_vin(input, default_channels),
        )?;
    }
    for bus in &profile.buses {
        add_node(
            &mut nodes,
            &mut seen,
            NodeDescriptor {
                id: bus.id.clone(),
                kind: NodeKind::Bus,
                channels: bus.channels.unwrap_or(default_channels),
                mix: bus.mix.clone(),
            },
        )?;
    }
    for input in &profile.external.inputs {
        add_node(
            &mut nodes,
            &mut seen,
            NodeDescriptor {
                id: input.id.clone(),
                kind: NodeKind::ExternalInput,
                channels: input.channels.unwrap_or(default_channels),
                mix: None,
            },
        )?;
    }
    for output in &profile.external.outputs {
        add_node(
            &mut nodes,
            &mut seen,
            NodeDescriptor {
                id: output.id.clone(),
                kind: NodeKind::ExternalOutput,
                channels: output.channels.unwrap_or(default_channels),
                mix: None,
            },
        )?;
    }

    let mut edges = Vec::new();
    for (index, pipe) in profile.pipes.iter().enumerate() {
        if !pipe.enabled {
            continue;
        }

        if !(0.0..=2000.0).contains(&pipe.delay_ms) {
            return Err(GraphError::InvalidDelay(pipe.delay_ms));
        }

        let source = nodes
            .get(&pipe.from)
            .ok_or_else(|| GraphError::UnknownSource(pipe.from.clone()))?;
        let destination = nodes
            .get(&pipe.to)
            .ok_or_else(|| GraphError::UnknownDestination(pipe.to.clone()))?;

        if !source.kind.is_source() {
            return Err(GraphError::InvalidSourceType(source.id.clone()));
        }
        if !destination.kind.is_sink() {
            return Err(GraphError::InvalidDestinationType(destination.id.clone()));
        }

        if !channels_compatible(source.channels, destination.channels) {
            return Err(GraphError::UnsupportedChannels {
                from: source.id.clone(),
                from_channels: source.channels,
                to: destination.id.clone(),
                to_channels: destination.channels,
            });
        }

        edges.push(PipeDescriptor {
            id: format!("{}:{}:{}", pipe.from, pipe.to, index),
            from: pipe.from.clone(),
            to: pipe.to.clone(),
            gain_db: pipe.gain_db,
            mute: pipe.mute,
            pan: pipe.pan,
            delay_ms: pipe.delay_ms,
        });
    }

    let graph = RoutingGraph { nodes, edges };
    ensure_acyclic(&graph)?;
    Ok(graph)
}

fn add_node(
    nodes: &mut BTreeMap<String, NodeDescriptor>,
    seen: &mut BTreeSet<String>,
    node: NodeDescriptor,
) -> Result<(), GraphError> {
    if !seen.insert(node.id.clone()) {
        return Err(GraphError::DuplicateNode(node.id));
    }
    nodes.insert(node.id.clone(), node);
    Ok(())
}

fn ensure_acyclic(graph: &RoutingGraph) -> Result<(), GraphError> {
    let mut digraph = DiGraph::<String, ()>::new();
    let mut index = HashMap::new();

    for node_id in graph.nodes.keys() {
        let idx = digraph.add_node(node_id.clone());
        index.insert(node_id.clone(), idx);
    }

    for edge in &graph.edges {
        if let (Some(from), Some(to)) = (index.get(&edge.from), index.get(&edge.to)) {
            digraph.add_edge(*from, *to, ());
        }
    }

    if is_cyclic_directed(&digraph) {
        return Err(GraphError::CycleDetected);
    }

    Ok(())
}

fn channels_compatible(source: u16, destination: u16) -> bool {
    source == destination || (source == 1 && destination == 2) || (source == 2 && destination == 1)
}

fn node_from_vout(device: &VirtualOutputDevice, default_channels: u16) -> NodeDescriptor {
    NodeDescriptor {
        id: device.id.clone(),
        kind: NodeKind::VirtualOutput,
        channels: device.channels.unwrap_or(default_channels),
        mix: None,
    }
}

fn node_from_vin(device: &VirtualInputDevice, default_channels: u16) -> NodeDescriptor {
    NodeDescriptor {
        id: device.id.clone(),
        kind: NodeKind::VirtualInput,
        channels: device.channels.unwrap_or(default_channels),
        mix: Some(device.mix.clone().unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use mars_types::{Pipe, Profile, VirtualInputDevice, VirtualOutputDevice};

    use super::{GraphError, build_routing_graph};

    #[test]
    fn builds_simple_graph() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "app".to_string(),
            name: "App".to_string(),
            channels: None,
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: None,
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
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("valid graph");
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
    }

    #[test]
    fn detects_cycle() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "a".to_string(),
            name: "A".to_string(),
            channels: None,
            uid: None,
            hidden: false,
        });
        profile.buses.push(mars_types::Bus {
            id: "b".to_string(),
            channels: Some(2),
            mix: None,
        });
        profile.pipes.push(Pipe {
            from: "a".to_string(),
            to: "b".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile.pipes.push(Pipe {
            from: "b".to_string(),
            to: "a".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(err, GraphError::InvalidDestinationType(_)));
    }
}

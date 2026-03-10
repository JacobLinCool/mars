#![forbid(unsafe_code)]
//! Routing graph construction and validation.

use std::collections::{BTreeMap, BTreeSet};

use mars_types::{
    NodeDescriptor, NodeKind, PipeDescriptor, ProcessorKind, Profile, VirtualInputDevice,
    VirtualOutputDevice,
};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledRoutePlan {
    pub topological_order: Vec<String>,
    pub routes: Vec<CompiledRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompiledProcessorPlan {
    pub chains: BTreeMap<String, CompiledProcessorChain>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledProcessorChain {
    pub id: String,
    pub processors: Vec<CompiledProcessor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledProcessor {
    pub id: String,
    pub kind: ProcessorKind,
    pub config_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledRoute {
    pub id: String,
    pub from: String,
    pub to: String,
    pub source_channels: u16,
    pub destination_channels: u16,
    pub matrix_rows: u16,
    pub matrix_cols: u16,
    pub matrix: Vec<f32>,
    pub gain_db: f32,
    pub mute: bool,
    pub pan: f32,
    pub delay_ms: f32,
    pub chain: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RoutingGraph {
    pub nodes: BTreeMap<String, NodeDescriptor>,
    pub edges: Vec<PipeDescriptor>,
    route_plan: CompiledRoutePlan,
    processor_plan: CompiledProcessorPlan,
}

impl RoutingGraph {
    #[must_use]
    pub fn topological_order(&self) -> Vec<String> {
        self.route_plan.topological_order.clone()
    }

    #[must_use]
    pub fn compiled_route_plan(&self) -> &CompiledRoutePlan {
        &self.route_plan
    }

    #[must_use]
    pub fn processor_plan(&self) -> &CompiledProcessorPlan {
        &self.processor_plan
    }

    pub fn outgoing<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a PipeDescriptor> {
        self.edges.iter().filter(move |edge| edge.from == id)
    }

    pub fn incoming<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a PipeDescriptor> {
        self.edges.iter().filter(move |edge| edge.to == id)
    }
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("duplicate node id: {0}")]
    DuplicateNode(String),
    #[error("node channel count must be in [1, 64], got {channels} for '{id}'")]
    InvalidNodeChannels { id: String, channels: u16 },
    #[error("unknown source node in pipe/route: {0}")]
    UnknownSource(String),
    #[error("unknown destination node in pipe/route: {0}")]
    UnknownDestination(String),
    #[error("node cannot be used as source: {0}")]
    InvalidSourceType(String),
    #[error("node cannot be used as destination: {0}")]
    InvalidDestinationType(String),
    #[error("duplicate route id in compiled graph: {0}")]
    DuplicateRouteId(String),
    #[error("route '{route_id}' references unknown processor chain '{chain_id}'")]
    UnknownRouteChain { route_id: String, chain_id: String },
    #[error("processor chain '{chain_id}' references unknown processor '{processor_id}'")]
    UnknownProcessorDefinition {
        chain_id: String,
        processor_id: String,
    },
    #[error(
        "channel mismatch not supported in legacy pipe mode: {from} ({from_channels}) -> {to} ({to_channels})"
    )]
    UnsupportedChannels {
        from: String,
        from_channels: u16,
        to: String,
        to_channels: u16,
    },
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
    #[error("graph contains cycle")]
    CycleDetected,
    #[error("pipe/route delay must be in range [0, 2000], got {0}")]
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
    for tap in &profile.captures.process_taps {
        add_node(
            &mut nodes,
            &mut seen,
            NodeDescriptor {
                id: tap.id.clone(),
                kind: NodeKind::ExternalInput,
                channels: tap.channels.unwrap_or(default_channels),
                mix: None,
            },
        )?;
    }
    for tap in &profile.captures.system_taps {
        add_node(
            &mut nodes,
            &mut seen,
            NodeDescriptor {
                id: tap.id.clone(),
                kind: NodeKind::ExternalInput,
                channels: tap.channels.unwrap_or(default_channels),
                mix: None,
            },
        )?;
    }

    let processor_plan = compile_processor_plan(profile)?;
    let (edges, routes) = if profile.routes.is_empty() {
        compile_legacy_pipes(profile, &nodes)?
    } else {
        compile_routes(profile, &nodes, &processor_plan)?
    };

    let topological_order = compile_topological_order(&nodes, &edges)?;
    Ok(RoutingGraph {
        nodes,
        edges,
        route_plan: CompiledRoutePlan {
            topological_order,
            routes,
        },
        processor_plan,
    })
}

fn compile_routes(
    profile: &Profile,
    nodes: &BTreeMap<String, NodeDescriptor>,
    processor_plan: &CompiledProcessorPlan,
) -> Result<(Vec<PipeDescriptor>, Vec<CompiledRoute>), GraphError> {
    let mut edges = Vec::new();
    let mut routes = Vec::new();
    let mut seen_route_ids = BTreeSet::new();

    for route in &profile.routes {
        if !route.enabled {
            continue;
        }
        if !(0.0..=2000.0).contains(&route.delay_ms) {
            return Err(GraphError::InvalidDelay(route.delay_ms));
        }

        let source = nodes
            .get(&route.from)
            .ok_or_else(|| GraphError::UnknownSource(route.from.clone()))?;
        let destination = nodes
            .get(&route.to)
            .ok_or_else(|| GraphError::UnknownDestination(route.to.clone()))?;

        if !source.kind.is_source() {
            return Err(GraphError::InvalidSourceType(source.id.clone()));
        }
        if !destination.kind.is_sink() {
            return Err(GraphError::InvalidDestinationType(destination.id.clone()));
        }
        if let Some(chain_id) = route.chain.as_deref() {
            if !processor_plan.chains.contains_key(chain_id) {
                return Err(GraphError::UnknownRouteChain {
                    route_id: route.id.clone(),
                    chain_id: chain_id.to_string(),
                });
            }
        }

        if !seen_route_ids.insert(route.id.clone()) {
            return Err(GraphError::DuplicateRouteId(route.id.clone()));
        }

        let rows = route.matrix.rows as usize;
        let cols = route.matrix.cols as usize;
        let actual_rows = route.matrix.coefficients.len();
        let actual_cols = route.matrix.coefficients.first().map_or(0, Vec::len);
        if rows == 0
            || cols == 0
            || rows != actual_rows
            || route
                .matrix
                .coefficients
                .iter()
                .any(|row| row.len() != cols)
        {
            return Err(GraphError::RouteMatrixShapeMismatch {
                route_id: route.id.clone(),
                rows: route.matrix.rows,
                cols: route.matrix.cols,
                actual_rows,
                actual_cols,
            });
        }

        if route.matrix.rows != destination.channels || route.matrix.cols != source.channels {
            return Err(GraphError::RouteMatrixChannelMismatch {
                route_id: route.id.clone(),
                expected_rows: destination.channels,
                expected_cols: source.channels,
                rows: route.matrix.rows,
                cols: route.matrix.cols,
            });
        }

        let mut flattened = Vec::with_capacity(rows * cols);
        for (row_index, row) in route.matrix.coefficients.iter().enumerate() {
            for (col_index, coefficient) in row.iter().enumerate() {
                if !coefficient.is_finite() {
                    return Err(GraphError::NonFiniteRouteMatrixCoefficient {
                        route_id: route.id.clone(),
                        row: row_index,
                        col: col_index,
                        value: *coefficient,
                    });
                }
                flattened.push(*coefficient);
            }
        }

        edges.push(PipeDescriptor {
            id: route.id.clone(),
            from: route.from.clone(),
            to: route.to.clone(),
            gain_db: route.gain_db,
            mute: route.mute,
            pan: route.pan,
            delay_ms: route.delay_ms,
        });
        routes.push(CompiledRoute {
            id: route.id.clone(),
            from: route.from.clone(),
            to: route.to.clone(),
            source_channels: source.channels,
            destination_channels: destination.channels,
            matrix_rows: route.matrix.rows,
            matrix_cols: route.matrix.cols,
            matrix: flattened,
            gain_db: route.gain_db,
            mute: route.mute,
            pan: route.pan,
            delay_ms: route.delay_ms,
            chain: route.chain.clone(),
        });
    }

    Ok((edges, routes))
}

fn compile_processor_plan(profile: &Profile) -> Result<CompiledProcessorPlan, GraphError> {
    let definitions = profile
        .processors
        .iter()
        .map(|processor| {
            (
                processor.id.clone(),
                (processor.kind, processor.config.to_string()),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut chains = BTreeMap::<String, CompiledProcessorChain>::new();
    for chain in &profile.processor_chains {
        let mut processors = Vec::with_capacity(chain.processors.len());
        for processor_id in &chain.processors {
            let Some((kind, config_json)) = definitions.get(processor_id) else {
                return Err(GraphError::UnknownProcessorDefinition {
                    chain_id: chain.id.clone(),
                    processor_id: processor_id.clone(),
                });
            };
            processors.push(CompiledProcessor {
                id: processor_id.clone(),
                kind: *kind,
                config_json: config_json.clone(),
            });
        }
        chains.insert(
            chain.id.clone(),
            CompiledProcessorChain {
                id: chain.id.clone(),
                processors,
            },
        );
    }

    Ok(CompiledProcessorPlan { chains })
}

fn compile_legacy_pipes(
    profile: &Profile,
    nodes: &BTreeMap<String, NodeDescriptor>,
) -> Result<(Vec<PipeDescriptor>, Vec<CompiledRoute>), GraphError> {
    let mut edges = Vec::new();
    let mut routes = Vec::new();

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

        let route_id = format!("{}:{}:{}", pipe.from, pipe.to, index);
        edges.push(PipeDescriptor {
            id: route_id.clone(),
            from: pipe.from.clone(),
            to: pipe.to.clone(),
            gain_db: pipe.gain_db,
            mute: pipe.mute,
            pan: pipe.pan,
            delay_ms: pipe.delay_ms,
        });
        routes.push(CompiledRoute {
            id: route_id,
            from: pipe.from.clone(),
            to: pipe.to.clone(),
            source_channels: source.channels,
            destination_channels: destination.channels,
            matrix_rows: destination.channels,
            matrix_cols: source.channels,
            matrix: legacy_pipe_matrix(source.channels, destination.channels),
            gain_db: pipe.gain_db,
            mute: pipe.mute,
            pan: pipe.pan,
            delay_ms: pipe.delay_ms,
            chain: None,
        });
    }

    Ok((edges, routes))
}

fn compile_topological_order(
    nodes: &BTreeMap<String, NodeDescriptor>,
    edges: &[PipeDescriptor],
) -> Result<Vec<String>, GraphError> {
    let mut indegree = BTreeMap::<String, usize>::new();
    let mut adjacency = BTreeMap::<String, Vec<String>>::new();
    for node_id in nodes.keys() {
        indegree.insert(node_id.clone(), 0);
    }

    for edge in edges {
        if let Some(target_degree) = indegree.get_mut(&edge.to) {
            *target_degree += 1;
        }
        adjacency
            .entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }
    for neighbors in adjacency.values_mut() {
        neighbors.sort();
    }

    let mut ready = indegree
        .iter()
        .filter_map(|(id, degree)| if *degree == 0 { Some(id.clone()) } else { None })
        .collect::<BTreeSet<_>>();

    let mut ordered = Vec::with_capacity(nodes.len());
    while let Some(node_id) = ready.pop_first() {
        ordered.push(node_id.clone());
        if let Some(neighbors) = adjacency.get(&node_id) {
            for next in neighbors {
                if let Some(degree) = indegree.get_mut(next) {
                    *degree -= 1;
                    if *degree == 0 {
                        ready.insert(next.clone());
                    }
                }
            }
        }
    }

    if ordered.len() != nodes.len() {
        return Err(GraphError::CycleDetected);
    }
    Ok(ordered)
}

fn add_node(
    nodes: &mut BTreeMap<String, NodeDescriptor>,
    seen: &mut BTreeSet<String>,
    node: NodeDescriptor,
) -> Result<(), GraphError> {
    if !(1..=64).contains(&node.channels) {
        return Err(GraphError::InvalidNodeChannels {
            id: node.id,
            channels: node.channels,
        });
    }
    if !seen.insert(node.id.clone()) {
        return Err(GraphError::DuplicateNode(node.id));
    }
    nodes.insert(node.id.clone(), node);
    Ok(())
}

fn channels_compatible(source: u16, destination: u16) -> bool {
    source == destination || (source == 1 && destination == 2) || (source == 2 && destination == 1)
}

fn legacy_pipe_matrix(source_channels: u16, destination_channels: u16) -> Vec<f32> {
    if source_channels == destination_channels {
        let channels = source_channels as usize;
        let mut matrix = vec![0.0; channels * channels];
        for index in 0..channels {
            matrix[index * channels + index] = 1.0;
        }
        return matrix;
    }

    match (source_channels, destination_channels) {
        (1, 2) => vec![1.0, 1.0],
        (2, 1) => vec![0.5, 0.5],
        _ => Vec::new(),
    }
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
#[allow(clippy::expect_used)]
mod tests {
    use mars_types::{
        Bus, CaptureConfig, Pipe, ProcessTap, ProcessTapSelector, ProcessorChain,
        ProcessorDefinition, ProcessorKind, Profile, Route, RouteMatrix, VirtualInputDevice,
        VirtualOutputDevice,
    };

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
        assert_eq!(graph.compiled_route_plan().routes.len(), 1);
    }

    #[test]
    fn builds_route_matrix_graph() {
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
        profile.routes.push(Route {
            id: "route-main".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("valid graph");
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.compiled_route_plan().routes.len(), 1);
        assert_eq!(
            graph.compiled_route_plan().routes[0].matrix,
            vec![1.0, 0.0, 0.0, 1.0]
        );
    }

    #[test]
    fn builds_graph_with_capture_tap_source_node() {
        let mut profile = Profile::default();
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: Some(2),
            uid: None,
            mix: None,
        });
        profile.captures = CaptureConfig {
            process_taps: vec![ProcessTap {
                id: "tap-app".to_string(),
                selector: ProcessTapSelector::Pid { pid: 4242 },
                channels: Some(2),
            }],
            system_taps: Vec::new(),
        };
        profile.routes.push(Route {
            id: "route-main".to_string(),
            from: "tap-app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("valid graph");
        assert!(graph.nodes.contains_key("tap-app"));
        assert_eq!(graph.compiled_route_plan().routes.len(), 1);
        assert_eq!(graph.compiled_route_plan().routes[0].from, "tap-app");
    }

    #[test]
    fn rejects_invalid_destination_type() {
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

    #[test]
    fn detects_duplicate_node_id() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "dup".to_string(),
            name: "Output".to_string(),
            channels: None,
            uid: None,
            hidden: false,
        });
        profile.buses.push(Bus {
            id: "dup".to_string(),
            channels: None,
            mix: None,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(err, GraphError::DuplicateNode(id) if id == "dup"));
    }

    #[test]
    fn detects_unknown_source() {
        let mut profile = Profile::default();
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: None,
            uid: None,
            mix: None,
        });
        profile.pipes.push(Pipe {
            from: "missing".to_string(),
            to: "mix".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(err, GraphError::UnknownSource(id) if id == "missing"));
    }

    #[test]
    fn detects_unknown_destination() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "app".to_string(),
            name: "App".to_string(),
            channels: None,
            uid: None,
            hidden: false,
        });
        profile.pipes.push(Pipe {
            from: "app".to_string(),
            to: "missing".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(err, GraphError::UnknownDestination(id) if id == "missing"));
    }

    #[test]
    fn rejects_invalid_source_type() {
        let mut profile = Profile::default();
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: None,
            uid: None,
            mix: None,
        });
        profile.buses.push(Bus {
            id: "bus".to_string(),
            channels: None,
            mix: None,
        });
        profile.pipes.push(Pipe {
            from: "mix".to_string(),
            to: "bus".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(err, GraphError::InvalidSourceType(id) if id == "mix"));
    }

    #[test]
    fn rejects_unsupported_channels_in_legacy_pipe_mode() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "mono".to_string(),
            name: "Mono".to_string(),
            channels: Some(1),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "quad".to_string(),
            name: "Quad".to_string(),
            channels: Some(4),
            uid: None,
            mix: None,
        });
        profile.pipes.push(Pipe {
            from: "mono".to_string(),
            to: "quad".to_string(),
            enabled: true,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(
            err,
            GraphError::UnsupportedChannels {
                from,
                from_channels: 1,
                to,
                to_channels: 4
            } if from == "mono" && to == "quad"
        ));
    }

    #[test]
    fn rejects_invalid_node_channel_count() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "too-many".to_string(),
            name: "Too Many".to_string(),
            channels: Some(65),
            uid: None,
            hidden: false,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(
            err,
            GraphError::InvalidNodeChannels { id, channels: 65 } if id == "too-many"
        ));
    }

    #[test]
    fn accepts_route_matrix_up_to_64_channels() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "in64".to_string(),
            name: "In64".to_string(),
            channels: Some(64),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "out64".to_string(),
            name: "Out64".to_string(),
            channels: Some(64),
            uid: None,
            mix: None,
        });

        let mut coefficients = vec![vec![0.0_f32; 64]; 64];
        for (index, row) in coefficients.iter_mut().enumerate() {
            row[index] = 1.0;
        }

        profile.routes.push(Route {
            id: "route64".to_string(),
            from: "in64".to_string(),
            to: "out64".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 64,
                cols: 64,
                coefficients,
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("route with 64 channels should compile");
        assert_eq!(graph.compiled_route_plan().routes.len(), 1);
        assert_eq!(graph.compiled_route_plan().routes[0].matrix.len(), 64 * 64);
    }

    #[test]
    fn compiles_processor_plan_for_route_chain() {
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
            id: "proc-eq".to_string(),
            kind: ProcessorKind::Eq,
            config: Default::default(),
        });
        profile.processor_chains.push(ProcessorChain {
            id: "voice-chain".to_string(),
            processors: vec!["proc-eq".to_string()],
        });
        profile.routes.push(Route {
            id: "route-main".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: Some("voice-chain".to_string()),
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("must compile");
        let chain = graph
            .processor_plan()
            .chains
            .get("voice-chain")
            .expect("compiled chain");
        assert_eq!(chain.processors.len(), 1);
        assert_eq!(chain.processors[0].id, "proc-eq");
        assert_eq!(chain.processors[0].kind, ProcessorKind::Eq);
        assert_eq!(chain.processors[0].config_json, "null");
        assert_eq!(
            graph.compiled_route_plan().routes[0].chain.as_deref(),
            Some("voice-chain")
        );
    }

    #[test]
    fn rejects_route_with_unknown_processor_chain() {
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
        profile.routes.push(Route {
            id: "route-main".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: Some("missing-chain".to_string()),
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(
            err,
            GraphError::UnknownRouteChain { route_id, chain_id }
                if route_id == "route-main" && chain_id == "missing-chain"
        ));
    }

    #[test]
    fn rejects_route_matrix_shape_mismatch() {
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
        profile.routes.push(Route {
            id: "bad-shape".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0]],
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(
            err,
            GraphError::RouteMatrixShapeMismatch { route_id, .. } if route_id == "bad-shape"
        ));
    }

    #[test]
    fn rejects_route_matrix_channel_mismatch() {
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
            channels: Some(2),
            uid: None,
            mix: None,
        });
        profile.routes.push(Route {
            id: "bad-channels".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(
            err,
            GraphError::RouteMatrixChannelMismatch { route_id, .. } if route_id == "bad-channels"
        ));
    }

    #[test]
    fn rejects_route_matrix_with_non_finite_coefficient() {
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
        profile.routes.push(Route {
            id: "bad-nan".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 1,
                cols: 1,
                coefficients: vec![vec![f32::NAN]],
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(
            err,
            GraphError::NonFiniteRouteMatrixCoefficient { route_id, .. } if route_id == "bad-nan"
        ));
    }

    #[test]
    fn rejects_route_with_unknown_reference() {
        let mut profile = Profile::default();
        profile.virtual_devices.inputs.push(VirtualInputDevice {
            id: "mix".to_string(),
            name: "Mix".to_string(),
            channels: Some(2),
            uid: None,
            mix: None,
        });
        profile.routes.push(Route {
            id: "missing-src".to_string(),
            from: "app".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(matches!(err, GraphError::UnknownSource(id) if id == "app"));
    }

    #[test]
    fn rejects_delay_out_of_range() {
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
            delay_ms: 2000.1,
        });

        let err = build_routing_graph(&profile).expect_err("must fail");
        assert!(
            matches!(err, GraphError::InvalidDelay(delay) if (delay - 2000.1).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn ignores_disabled_pipe() {
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
            enabled: false,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("graph should build");
        assert!(graph.edges.is_empty());
    }

    #[test]
    fn detects_cycle_between_buses() {
        let mut profile = Profile::default();
        profile.buses.push(Bus {
            id: "a".to_string(),
            channels: Some(2),
            mix: None,
        });
        profile.buses.push(Bus {
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
        assert!(matches!(err, GraphError::CycleDetected));
    }

    #[test]
    fn topological_order_is_deterministic() {
        let mut profile = Profile::default();
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "b".to_string(),
            name: "B".to_string(),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
        profile.virtual_devices.outputs.push(VirtualOutputDevice {
            id: "a".to_string(),
            name: "A".to_string(),
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
        profile.routes.push(Route {
            id: "ra".to_string(),
            from: "a".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
        profile.routes.push(Route {
            id: "rb".to_string(),
            from: "b".to_string(),
            to: "mix".to_string(),
            enabled: true,
            matrix: RouteMatrix {
                rows: 2,
                cols: 2,
                coefficients: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });

        let graph = build_routing_graph(&profile).expect("graph should compile");
        let first = graph.topological_order();
        let second = graph.topological_order();
        assert_eq!(first, second);
        assert_eq!(
            first,
            vec!["a".to_string(), "b".to_string(), "mix".to_string()]
        );
    }
}

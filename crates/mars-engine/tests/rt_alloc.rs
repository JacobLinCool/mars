#![allow(clippy::expect_used)]

use std::collections::HashMap;

use mars_engine::{Engine, EngineSnapshot};
use mars_graph::build_routing_graph;
use mars_types::{Profile, Route, RouteMatrix, VirtualInputDevice, VirtualOutputDevice};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<std::alloc::System> = &INSTRUMENTED_SYSTEM;

fn identity_matrix(channels: u16) -> RouteMatrix {
    let channels = channels as usize;
    let mut coefficients = vec![vec![0.0; channels]; channels];
    for (index, row) in coefficients.iter_mut().enumerate() {
        row[index] = 1.0;
    }

    RouteMatrix {
        rows: channels as u16,
        cols: channels as u16,
        coefficients,
    }
}

#[test]
fn render_cycle_into_has_zero_heap_allocation_after_prepare() {
    let mut profile = Profile::default();
    profile.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app".to_string(),
        name: "App".to_string(),
        channels: Some(8),
        uid: None,
        hidden: false,
    });
    profile.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".to_string(),
        name: "Mix".to_string(),
        channels: Some(8),
        uid: None,
        mix: None,
    });
    profile.routes.push(Route {
        id: "matrix-main".to_string(),
        from: "app".to_string(),
        to: "mix".to_string(),
        enabled: true,
        matrix: identity_matrix(8),
        chain: None,
        gain_db: 0.0,
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

    let mut sources = HashMap::new();
    let source_samples = (0..(256 * 8))
        .map(|index| ((index as f32) * 0.001).sin())
        .collect::<Vec<_>>();
    sources.insert("app".to_string(), source_samples);

    let mut sink_outputs = HashMap::new();
    sink_outputs.insert("mix".to_string(), vec![0.0; 256 * 8]);

    // Warm up twice so all one-time buffers and lazy runtime internals are prepared.
    engine
        .render_cycle_into(256, &sources, &mut sink_outputs)
        .expect("warmup render");
    engine
        .render_cycle_into(256, &sources, &mut sink_outputs)
        .expect("warmup render #2");

    let region = Region::new(&GLOBAL);
    engine
        .render_cycle_into(256, &sources, &mut sink_outputs)
        .expect("measured render");
    let delta = region.change();

    assert_eq!(delta.allocations, 0);
    assert_eq!(delta.reallocations, 0);
    assert_eq!(delta.deallocations, 0);
}

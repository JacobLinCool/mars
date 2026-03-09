#![allow(clippy::expect_used, clippy::unwrap_used)]

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use mars_graph::build_routing_graph;
use mars_types::{
    Bus, MixConfig, MixMode, Profile, Route, RouteMatrix, VirtualInputDevice, VirtualOutputDevice,
};

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

fn small_profile() -> Profile {
    let mut p = Profile::default();
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app".into(),
        name: "App".into(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix".into(),
        name: "Mix".into(),
        channels: Some(2),
        uid: None,
        mix: None,
    });
    p.routes.push(Route {
        id: "r-app-mix".into(),
        from: "app".into(),
        to: "mix".into(),
        enabled: true,
        matrix: identity_matrix(2),
        chain: None,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p
}

fn medium_profile() -> Profile {
    let mut p = Profile::default();
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app1".into(),
        name: "App 1".into(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    p.virtual_devices.outputs.push(VirtualOutputDevice {
        id: "app2".into(),
        name: "App 2".into(),
        channels: Some(2),
        uid: None,
        hidden: false,
    });
    p.buses.push(Bus {
        id: "bus1".into(),
        channels: Some(2),
        mix: Some(MixConfig {
            limiter: true,
            limit_dbfs: -1.0,
            mode: MixMode::Sum,
        }),
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix1".into(),
        name: "Mix 1".into(),
        channels: Some(2),
        uid: None,
        mix: None,
    });
    p.virtual_devices.inputs.push(VirtualInputDevice {
        id: "mix2".into(),
        name: "Mix 2".into(),
        channels: Some(2),
        uid: None,
        mix: None,
    });
    p.routes.push(Route {
        id: "r-app1-bus1".into(),
        from: "app1".into(),
        to: "bus1".into(),
        enabled: true,
        matrix: identity_matrix(2),
        chain: None,
        gain_db: -6.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p.routes.push(Route {
        id: "r-app2-bus1".into(),
        from: "app2".into(),
        to: "bus1".into(),
        enabled: true,
        matrix: identity_matrix(2),
        chain: None,
        gain_db: -3.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p.routes.push(Route {
        id: "r-bus1-mix1".into(),
        from: "bus1".into(),
        to: "mix1".into(),
        enabled: true,
        matrix: identity_matrix(2),
        chain: None,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p.routes.push(Route {
        id: "r-bus1-mix2".into(),
        from: "bus1".into(),
        to: "mix2".into(),
        enabled: true,
        matrix: identity_matrix(2),
        chain: None,
        gain_db: 0.0,
        mute: false,
        pan: 0.0,
        delay_ms: 0.0,
    });
    p
}

fn large_profile() -> Profile {
    let mut p = Profile::default();
    for i in 0..5 {
        p.virtual_devices.outputs.push(VirtualOutputDevice {
            id: format!("app{i}"),
            name: format!("App {i}"),
            channels: Some(2),
            uid: None,
            hidden: false,
        });
    }
    for i in 0..5 {
        p.buses.push(Bus {
            id: format!("bus{i}"),
            channels: Some(2),
            mix: Some(MixConfig {
                limiter: i % 2 == 0,
                limit_dbfs: -1.0,
                mode: MixMode::Sum,
            }),
        });
    }
    for i in 0..5 {
        p.virtual_devices.inputs.push(VirtualInputDevice {
            id: format!("mix{i}"),
            name: format!("Mix {i}"),
            channels: Some(2),
            uid: None,
            mix: None,
        });
    }
    // app -> bus
    for i in 0..5 {
        p.routes.push(Route {
            id: format!("r-app{i}-bus{i}"),
            from: format!("app{i}"),
            to: format!("bus{i}"),
            enabled: true,
            matrix: identity_matrix(2),
            chain: None,
            gain_db: -6.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
    }
    // cross connections: app -> other buses
    for i in 0..5 {
        let target = (i + 1) % 5;
        p.routes.push(Route {
            id: format!("r-app{i}-bus{target}"),
            from: format!("app{i}"),
            to: format!("bus{target}"),
            enabled: true,
            matrix: identity_matrix(2),
            chain: None,
            gain_db: -12.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
    }
    // bus -> mix
    for i in 0..5 {
        p.routes.push(Route {
            id: format!("r-bus{i}-mix{i}"),
            from: format!("bus{i}"),
            to: format!("mix{i}"),
            enabled: true,
            matrix: identity_matrix(2),
            chain: None,
            gain_db: 0.0,
            mute: false,
            pan: 0.0,
            delay_ms: 0.0,
        });
    }
    p
}

fn bench_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/build");

    let small = small_profile();
    let medium = medium_profile();
    let large = large_profile();

    group.bench_with_input(BenchmarkId::new("small", "2n1e"), &small, |b, p| {
        b.iter(|| build_routing_graph(p).unwrap());
    });
    group.bench_with_input(BenchmarkId::new("medium", "5n4e"), &medium, |b, p| {
        b.iter(|| build_routing_graph(p).unwrap());
    });
    group.bench_with_input(BenchmarkId::new("large", "15n15e"), &large, |b, p| {
        b.iter(|| build_routing_graph(p).unwrap());
    });

    group.finish();
}

fn bench_topological_order(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph/topological_order");

    let graph = build_routing_graph(&large_profile()).unwrap();

    group.bench_function("large", |b| {
        b.iter(|| graph.topological_order());
    });

    group.finish();
}

criterion_group!(benches, bench_build, bench_topological_order);
criterion_main!(benches);

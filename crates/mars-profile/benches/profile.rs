#![allow(clippy::expect_used, clippy::unwrap_used)]

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use mars_profile::{parse_profile_str, validate_profile};

const SMALL_YAML: &str = r#"
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
    - id: mix1
      name: Mix 1
routes:
  - id: route1
    from: app1
    to: mix1
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
processors:
  - id: eq-main
    kind: eq
processor_chains:
  - id: main-chain
    processors:
      - eq-main
captures:
  process_taps:
    - id: tap-app1
      selector:
        type: bundle_id
        bundle_id: com.example.app1
  system_taps:
    - id: tap-system
      mode: default_output
sinks:
  files:
    - id: rec-main
      path: /tmp/mars-small.wav
      format: wav
  streams:
    - id: stream-main
      transport: rtp
      endpoint: rtp://127.0.0.1:5004
pipes:
  - from: app1
    to: mix1
"#;

const MEDIUM_YAML: &str = r#"
version: 2
audio:
  sample_rate: 48000
  channels: 2
  buffer_frames: 256
virtual:
  outputs:
    - id: app1
      name: App 1
    - id: app2
      name: App 2
  inputs:
    - id: mix1
      name: Mix 1
    - id: mix2
      name: Mix 2
buses:
  - id: bus1
    channels: 2
    mix:
      limiter: true
      limit_dbfs: -1.0
      mode: sum
routes:
  - id: route-bus-mix1
    from: bus1
    to: mix1
    chain: voice-chain
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
  - id: route-bus-mix2
    from: bus1
    to: mix2
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
processors:
  - id: eq-voice
    kind: eq
  - id: dynamics-voice
    kind: dynamics
processor_chains:
  - id: voice-chain
    processors:
      - eq-voice
      - dynamics-voice
captures:
  process_taps:
    - id: tap-app2
      selector:
        type: pid
        pid: 4567
  system_taps: []
sinks:
  files:
    - id: rec-medium
      path: /tmp/mars-medium.caf
      format: caf
  streams:
    - id: stream-medium
      transport: srt
      endpoint: srt://127.0.0.1:10080
pipes:
  - from: app1
    to: bus1
    gain_db: -6.0
  - from: app2
    to: bus1
    gain_db: -3.0
  - from: bus1
    to: mix1
  - from: bus1
    to: mix2
"#;

const LARGE_YAML: &str = r#"
version: 2
audio:
  sample_rate: 48000
  channels: 2
  buffer_frames: 256
virtual:
  outputs:
    - id: app0
      name: App 0
    - id: app1
      name: App 1
    - id: app2
      name: App 2
    - id: app3
      name: App 3
    - id: app4
      name: App 4
  inputs:
    - id: mix0
      name: Mix 0
    - id: mix1
      name: Mix 1
    - id: mix2
      name: Mix 2
    - id: mix3
      name: Mix 3
    - id: mix4
      name: Mix 4
buses:
  - id: bus0
    channels: 2
    mix:
      limiter: true
      limit_dbfs: -1.0
      mode: sum
  - id: bus1
    channels: 2
  - id: bus2
    channels: 2
    mix:
      limiter: true
      limit_dbfs: -1.0
      mode: sum
  - id: bus3
    channels: 2
  - id: bus4
    channels: 2
    mix:
      limiter: true
      limit_dbfs: -1.0
      mode: sum
routes:
  - id: route-bus0-mix0
    from: bus0
    to: mix0
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
  - id: route-bus1-mix1
    from: bus1
    to: mix1
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
  - id: route-bus2-mix2
    from: bus2
    to: mix2
    matrix:
      rows: 2
      cols: 2
      coefficients:
        - [1.0, 0.0]
        - [0.0, 1.0]
processors:
  - id: eq0
    kind: eq
  - id: denoise0
    kind: denoise
  - id: shift0
    kind: time_shift
processor_chains:
  - id: chain0
    processors:
      - eq0
      - denoise0
      - shift0
captures:
  process_taps:
    - id: tap-browser
      selector:
        type: bundle_id
        bundle_id: com.example.browser
  system_taps:
    - id: tap-system-main
      mode: all_output
sinks:
  files:
    - id: rec-large
      path: /tmp/mars-large.wav
      format: wav
  streams:
    - id: stream-large
      transport: webrtc
      endpoint: webrtc://localhost/session
pipes:
  - from: app0
    to: bus0
    gain_db: -6.0
  - from: app1
    to: bus1
    gain_db: -6.0
  - from: app2
    to: bus2
    gain_db: -6.0
  - from: app3
    to: bus3
    gain_db: -6.0
  - from: app4
    to: bus4
    gain_db: -6.0
  - from: app0
    to: bus1
    gain_db: -12.0
  - from: app1
    to: bus2
    gain_db: -12.0
  - from: app2
    to: bus3
    gain_db: -12.0
  - from: app3
    to: bus4
    gain_db: -12.0
  - from: app4
    to: bus0
    gain_db: -12.0
  - from: bus0
    to: mix0
  - from: bus1
    to: mix1
  - from: bus2
    to: mix2
  - from: bus3
    to: mix3
  - from: bus4
    to: mix4
"#;

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("profile/parse");

    group.bench_with_input(
        BenchmarkId::new("small", "1pipe"),
        &SMALL_YAML,
        |b, yaml| {
            b.iter(|| parse_profile_str(yaml).unwrap());
        },
    );
    group.bench_with_input(
        BenchmarkId::new("medium", "4pipes"),
        &MEDIUM_YAML,
        |b, yaml| {
            b.iter(|| parse_profile_str(yaml).unwrap());
        },
    );
    group.bench_with_input(
        BenchmarkId::new("large", "15pipes"),
        &LARGE_YAML,
        |b, yaml| {
            b.iter(|| parse_profile_str(yaml).unwrap());
        },
    );

    group.finish();
}

fn bench_validate(c: &mut Criterion) {
    let mut group = c.benchmark_group("profile/validate");

    group.bench_function("small", |b| {
        b.iter_batched(
            || parse_profile_str(SMALL_YAML).unwrap(),
            |profile| validate_profile(profile).unwrap(),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("medium", |b| {
        b.iter_batched(
            || parse_profile_str(MEDIUM_YAML).unwrap(),
            |profile| validate_profile(profile).unwrap(),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("large", |b| {
        b.iter_batched(
            || parse_profile_str(LARGE_YAML).unwrap(),
            |profile| validate_profile(profile).unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_parse, bench_validate);
criterion_main!(benches);

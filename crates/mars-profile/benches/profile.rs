#![allow(clippy::expect_used, clippy::unwrap_used)]

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use mars_profile::{parse_profile_str, validate_profile};

const SMALL_YAML: &str = r#"
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
    - id: mix1
      name: Mix 1
pipes:
  - from: app1
    to: mix1
"#;

const MEDIUM_YAML: &str = r#"
version: 1
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
version: 1
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

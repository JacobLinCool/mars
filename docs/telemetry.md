# MARS Telemetry (Optional OpenTelemetry)

MARS supports optional OpenTelemetry (OTLP) export for usage, reliability, and realtime performance analysis.

## Enablement model

Telemetry mode is controlled by `MARS_OTEL_MODE`:

- `off` (default): telemetry is disabled, no OTLP providers are initialized.
- `required`: telemetry must initialize successfully; process startup fails fast on configuration/init errors.

When `required` mode is used, `OTEL_EXPORTER_OTLP_ENDPOINT` must be set.

## Build with telemetry

Telemetry code paths are behind the crate feature `otel`.

```bash
cargo build --workspace --features otel
```

## Runtime configuration

Example:

```bash
export MARS_OTEL_MODE=required
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
```

Then run normal commands/binaries (`mars`, `marsd`, `mars-plugin-host`).

## Traces

- `mars.cli.command`
- `mars.ipc.client.request`
- `mars.daemon.request`
- `mars.apply.transaction`
- `mars.apply.stage.profile_validate`
- `mars.apply.stage.plan`
- `mars.apply.stage.external_resolve`
- `mars.apply.stage.driver_compatibility`
- `mars.apply.stage.driver_stage`
- `mars.apply.stage.graph_activate`
- `mars.apply.stage.capture_sync`
- `mars.apply.stage.render_sync`
- `mars.apply.stage.runtime_ready`

## Metrics

Control plane:

- `mars.cli.command.count`
- `mars.cli.command.duration`
- `mars.ipc.request.count`
- `mars.ipc.request.duration`
- `mars.daemon.request.count`
- `mars.daemon.request.duration`
- `mars.apply.count`
- `mars.apply.duration`
- `mars.apply.stage.duration`
- `mars.apply.rollback.count`

Realtime/runtime snapshots:

- `mars.render.cycle.duration`
- `mars.render.cycle.budget_utilization`
- `mars.render.deadline_miss.count`
- `mars.render.xrun.count`
- `mars.external.underrun.count`
- `mars.external.overrun.count`
- `mars.sink.drop.count`
- `mars.sink.write_error.count`
- `mars.capture.tap.active`
- `mars.capture.tap.failed`
- `mars.plugin.process.duration`
- `mars.plugin.timeout.count`
- `mars.plugin.error.count`
- `mars.plugin.restart.count`

## Privacy and cardinality

Telemetry intentionally does not export audio payload or raw high-cardinality identifiers such as:

- profile file paths
- sink file paths
- raw device UIDs
- process bundle IDs

Use bounded attributes (command/stage/success/sample_rate/buffer_frames/api) for stable dashboards.

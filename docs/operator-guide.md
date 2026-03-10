# MARS Operator Guide

## Profiles

Default profile directory:

- `~/Library/Application Support/mars/profiles/`

A profile is addressed by file name stem (`<name>.yaml`).

## Core commands

- `mars create <name> --template default|multi|blank`
- `mars open <name> [--editor "Visual Studio Code"]`
- `mars validate <name>`
- `mars plan <name>`
- `mars apply <name> [--dry-run] [--no-delete] [--timeout 5000]`
- `mars clear [--keep-devices]`
- `mars status [--json]`
- `mars devices [--json]`
- `mars processes [--json]`
- `mars logs [--follow]`
- `mars doctor [--json]`

## Runtime behavior notes

- External I/O uses `Degrade + Heal` at runtime:
  - Input endpoint disconnect: MARS fills silence and keeps routing active.
  - Output endpoint disconnect: MARS drops that endpoint's output and keeps routing active.
  - A background recovery supervisor retries reconnect with exponential backoff.
- `mars status --json` includes `external_runtime.degraded_inputs` and `external_runtime.degraded_outputs`.
- `mars status --json` also includes:
  - `graph_route_count`
  - `processor_runtime` (per-processor prepare/process/reset counters)
  - `capture_runtime` and `sink_runtime` health/counter snapshots
- `mars processes --json` lists process object id, pid, bundle id, and running I/O flags for capture selector authoring.
- `external_runtime.stream_errors` is capped (ring buffer) to avoid unbounded growth.

## Log cursor semantics

- `mars logs` / daemon `LogResponse.next_cursor` now represent a byte offset in `marsd.log`.
- `cursor=None` returns tail lines (default `200`) and a byte offset cursor.
- `cursor=<offset>` returns incremental lines from that byte position.
- If the log is rotated or truncated, the daemon falls back to tail mode automatically.

## External match semantics

- `uid`/`name`/`name_regex` remain strict match filters.
- `manufacturer`/`transport` are best-effort:
  - When metadata exists, it must match.
  - When metadata is unavailable, the candidate is treated as `unknown` (not immediate mismatch).
  - Matches that rely on unknown metadata emit warnings in `plan`/`apply`.

## Exit codes

- `0`: success
- `2`: invalid profile/schema/arguments
- `3`: missing external device with error policy
- `4`: driver unavailable/incompatible
- `5`: daemon communication failure
- `6`: apply failed
- `130`: interrupted

## Runtime paths

- socket: `~/Library/Caches/mars/marsd.sock`
- daemon log: `~/Library/Logs/mars/marsd.log`
- driver state cache: `~/Library/Caches/mars/driver_applied_state.json`

## Deployment flow

1. Build and install with `./scripts/install.sh`.
2. Run `mars doctor` to verify driver/daemon status.
3. Create and validate profiles.
4. Apply profile and verify with `mars status --json`.

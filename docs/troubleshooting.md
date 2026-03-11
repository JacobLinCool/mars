# MARS Troubleshooting

## `mars apply` returns exit code 4

Cause:

- `mars.driver` is not installed in `/Library/Audio/Plug-Ins/HAL`.

Fix:

1. Run `./scripts/install.sh`.
2. Reload CoreAudio: `sudo killall -9 coreaudiod`.
3. If still needed on your macOS build, reboot.
4. Re-run `mars doctor`.

## `mars` cannot reach daemon (exit 5)

Cause:

- `marsd` is not running or socket path is stale.

Fix:

1. Check `~/Library/Caches/mars/marsd.sock`.
2. Restart launch agent:
    - `launchctl kickstart -k gui/$UID/com.mars.marsd`
3. Check logs with `mars logs`.

## Missing external devices (exit 3)

Cause:

- Profile `external` match criteria cannot resolve available hardware.

Fix:

1. Run `mars devices --json` and inspect names/UIDs.
2. Update profile `match` fields (strict mode rejects `fallback`).
3. Re-run `mars validate` and `mars plan`.

Notes:

- `manufacturer` / `transport` matching is best-effort when metadata is unavailable on the host.
- If matching succeeded via unknown metadata, `plan` / `apply` warnings will mention it.

## Process tap selector did not match any process object

Symptom:

- `process tap '<id>' selector '<selector>' did not match any active CoreAudio process object`

Cause:

- The configured PID or bundle id does not match current CoreAudio process objects.

Fix:

1. Run `mars processes --json`.
2. Pick a selector that exists on the current host:
   - `type: pid` for a specific running process.
   - `type: bundle_id` for stable app matching.
3. Update profile `captures.process_taps`, then re-run `mars validate` and `mars apply`.

## Stream sink reports `not implemented`

Symptom:

- `stream sink not implemented for transport=...` appears in logs or sink status.

Cause:

- `sinks.streams` is descriptor-only in the current runtime; file sinks are implemented, stream sinks are not.

Fix:

1. Check runtime health:
   - `mars status --json` -> `sink_runtime.sinks[].health` and `sink_runtime.sinks[].last_error`
   - `mars doctor --json` -> `sink_failed` and `sink_write_errors`
2. Remove or disable `sinks.streams` entries.
3. Use `sinks.files` (`wav` or `caf`) for current recording output.

## AU plugin host shows timeouts/errors/restarts

Symptom:

- `plugin_timeouts`, `plugin_errors`, or `plugin_restarts` is non-zero in `mars doctor --json`.

Cause:

- The isolated AU host (`mars-plugin-host`) hit timeout/restart/error conditions.

Fix:

1. Inspect counters:
   - `mars status --json` -> `plugin_runtime.*` and `plugin_runtime.instances[]`
   - `mars doctor --json` -> `plugin_active`, `plugin_failed`, `plugin_timeouts`, `plugin_errors`, `plugin_restarts`
2. Check logs:
   - `mars logs`
3. Re-apply the profile after correcting AU config, then verify counters stabilize.

## External endpoint disconnects during runtime

Cause:

- An external input/output stream was interrupted (device unplugged, transport reset, CoreAudio stream error).

Behavior:

- MARS keeps the graph running and enters degraded mode for the failed endpoint(s).
- Input endpoint degraded: silence is injected.
- Output endpoint degraded: endpoint output is dropped.
- Background reconnect retries run with backoff.

Fix:

1. Check `mars status --json` fields:
   - `external_runtime.connected_inputs`
   - `external_runtime.connected_outputs`
   - `external_runtime.degraded_inputs`
   - `external_runtime.degraded_outputs`
2. Inspect recent stream errors:
   - `mars logs`
3. Reconnect the physical/virtual device and wait for retry, or re-apply profile if needed.

## `mars logs --follow` misses lines after log rotation

Cause:

- `marsd.log` was truncated or rotated while following.

Fix:

1. Re-run `mars logs --follow`; cursoring uses byte offsets and auto-recovers from truncation.
2. If needed, fetch a fresh tail snapshot:
   - `mars logs`

## No microphone audio from external input

Cause:

- Microphone permission missing.

Fix:

1. Open System Settings > Privacy & Security > Microphone.
2. Allow the terminal app (or host app running `marsd`).

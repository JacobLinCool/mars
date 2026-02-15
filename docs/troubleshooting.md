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

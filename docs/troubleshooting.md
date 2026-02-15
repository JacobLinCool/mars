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
2. Update profile `match`/`fallback` fields.
3. Re-run `mars validate` and `mars plan`.

## No microphone audio from external input

Cause:

- Microphone permission missing.

Fix:

1. Open System Settings > Privacy & Security > Microphone.
2. Allow the terminal app (or host app running `marsd`).

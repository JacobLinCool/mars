# MARS Getting Started

This guide covers a full dev-first setup on macOS, from first install to first profile apply.

## Prerequisites

- macOS (MARS uses CoreAudio HAL and launchd user agents).
- Rust toolchain (`cargo`, `rustc`) available in your shell.
- `sudo` access (needed for protected system paths like `/usr/local/bin` and `/Library/Audio/Plug-Ins/HAL`).
- Run commands from the repository root.

## 1. Build and Install

Run install as your normal user (do not run the whole script with `sudo`):

```bash
./scripts/install.sh
```

The installer may prompt for `sudo` only when needed. It installs:

- `/usr/local/bin/mars`
- `/usr/local/bin/marsd`
- `/Library/Audio/Plug-Ins/HAL/mars.driver`
- `~/Library/LaunchAgents/com.mars.marsd.plist`

## 2. Verify Installation

```bash
mars doctor
```

Optional checks:

```bash
launchctl print "gui/$UID/com.mars.marsd" | rg "state|pid"
mars logs
```

If `mars logs` shows `Mars driver plugin not found in loaded CoreAudio plugins`, reload CoreAudio once:

```bash
sudo killall -9 coreaudiod
mars doctor
```

## 3. Run Your First Profile

```bash
mars create demo
mars open demo
mars validate demo
mars plan demo
mars apply demo
mars status --json
```

## 4. Uninstall (if needed)

```bash
./scripts/uninstall.sh
```

Run as your normal user (the script prompts for elevated steps when required).

## Troubleshooting

### `Bootstrap failed: 125: Domain does not support specified action`

Cause:

- Install was run as root (`sudo ./scripts/install.sh`), so launchd targeted the wrong GUI domain.

Fix:

1. Run install as your normal user: `./scripts/install.sh`.

### `Permission denied` writing `~/Library/LaunchAgents/com.mars.marsd.plist`

Cause:

- A stale root-owned plist exists from a previous sudo run.

Fix:

1. Run install again; the script now auto-removes a non-writable stale plist.
2. If it still fails, remove it manually and retry:
   - `sudo rm -f ~/Library/LaunchAgents/com.mars.marsd.plist`
   - `./scripts/install.sh`

### `Permission denied` writing `/usr/local/bin/...`

Cause:

- `/usr/local/bin` is not user-writable on your system.

Fix:

1. Re-run `./scripts/install.sh` and approve the `sudo` prompt for binary install.

### `mars apply` exits with code `4`

Cause:

- Driver is not installed or not loaded.

Fix:

1. Run `./scripts/install.sh`.
2. Re-run `mars doctor`.
3. Reload CoreAudio and retry:
   - `sudo killall -9 coreaudiod`
4. If still needed on your macOS build, reboot.

### `mars` exits with code `5` (cannot reach daemon)

Cause:

- `marsd` is not running or socket state is stale.

Fix:

1. Restart the launch agent:
   - `launchctl kickstart -k gui/$UID/com.mars.marsd`
2. Check logs:
   - `mars logs`

## Useful References

- Operator guide: `docs/operator-guide.md`
- Additional troubleshooting: `docs/troubleshooting.md`
- Driver compatibility notes: `docs/driver-compatibility-matrix.md`

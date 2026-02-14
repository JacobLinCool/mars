# MARS

MARS (macOS Audio Router Service) is an audio routing system for macOS.

## What is included

- `mars` CLI with commands: `create`, `open`, `apply`, `clear`, `validate`, `plan`, `status`, `devices`, `logs`, `doctor`
- `marsd` daemon with declarative apply transaction and rollback semantics
- `mars-hal` AudioServerPlugIn driver crate and `mars.driver` bundle scaffold
- Shared profile schema, graph validator, ring-buffer model, and realtime engine core

## Build

```bash
cargo build
cargo test
```

## Install (dev-first)

```bash
./scripts/install.sh
```

This installs:

- `/usr/local/bin/mars`
- `/usr/local/bin/marsd`
- `/Library/Audio/Plug-Ins/HAL/mars.driver`
- `~/Library/LaunchAgents/com.mars.marsd.plist`

## Uninstall

```bash
./scripts/uninstall.sh
```

This removes:

- `/usr/local/bin/mars` and `/usr/local/bin/marsd`
- `/Library/Audio/Plug-Ins/HAL/mars.driver` (requires sudo)
- `~/Library/LaunchAgents/com.mars.marsd.plist`
- `~/Library/Caches/mars`

## Usage

```bash
mars create demo
mars open demo
mars validate demo
mars plan demo
mars apply demo
mars status --json
mars doctor
mars clear
```

## Development mode without system driver

```bash
export MARS_ALLOW_DRIVER_STUB=1
cargo run -p mars-daemon --bin marsd -- --serve
```

Then run CLI commands from another terminal.

## Logs

```bash
mars logs
./scripts/logs.sh
```

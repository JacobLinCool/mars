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
- `mars logs [--follow]`
- `mars doctor [--json]`

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

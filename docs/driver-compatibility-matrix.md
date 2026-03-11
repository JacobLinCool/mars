# Driver Compatibility Matrix

| Component       | Minimum                       | Tested                | Notes                              |
| --------------- | ----------------------------- | --------------------- | ---------------------------------- |
| macOS           | 15.0                          | 15.x (Sequoia family) | Target is macOS 15+                |
| Xcode           | 16.0                          | 16.3                  | Required for native toolchain      |
| Rust            | 1.87                          | 1.93                  | Workspace currently tested on 1.93 |
| HAL bundle path | `/Library/Audio/Plug-Ins/HAL` | same                  | Requires admin privileges          |

## Tap capability notes

- Process/system capture taps depend on CoreAudio tap API support on macOS 15+ hosts.
- Use `mars doctor` to verify tap capability on the current machine before enabling `captures.*` in profiles.

## Driver/daemon version check

- `marsd` stages driver state with explicit `driver_version`.
- `mars doctor` surfaces install and compatibility status.
- `marsd` now always enforces real HAL install/load checks during apply.
- `scripts/build-driver.sh` requires `Developer ID Application` by default.
- Local insecure signing is opt-in only via `MARS_ALLOW_INSECURE_SIGNING=1`.

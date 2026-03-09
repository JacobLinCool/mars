# Driver Compatibility Matrix

| Component       | Minimum                       | Tested                | Notes                              |
| --------------- | ----------------------------- | --------------------- | ---------------------------------- |
| macOS           | 14.0                          | 15.x (Sequoia family) | Target is macOS 14+                |
| Xcode           | 16.0                          | 16.3                  | Required for native toolchain      |
| Rust            | 1.87                          | 1.93                  | Workspace currently tested on 1.93 |
| HAL bundle path | `/Library/Audio/Plug-Ins/HAL` | same                  | Requires admin privileges          |

## Driver/daemon version check

- `marsd` stages driver state with explicit `driver_version`.
- `mars doctor` surfaces install and compatibility status.
- `marsd` now always enforces real HAL install/load checks during apply.
- `scripts/build-driver.sh` requires `Developer ID Application` by default.
- On SIP-disabled hosts, build/install auto-falls back to local insecure signing.
- You can always force fallback explicitly via `MARS_ALLOW_INSECURE_SIGNING=1`.

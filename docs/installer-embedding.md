# Embedding the MARS Runtime Installer

This guide is for Tauri/native macOS apps that ship MARS as a bundled
dependency and need a product-grade install/update/status flow. The
implementation lives in `mars-sdk::runtime`; the `mars runtime` CLI is a thin
wrapper over the same functions, so everything below can be driven either by
linking `mars-sdk` directly (recommended for Rust/Tauri) or by shelling out to
`mars runtime ... --json`.

## Runtime package

Build a relocatable, versioned package on your release machine:

```bash
./scripts/package-runtime.sh
# -> dist/mars-runtime-<version>.tar.gz
```

Package layout:

```
manifest.json                 # { version, min_macos, protocol_version, files: [...] }
bin/mars  bin/marsd
launchd/com.mars.marsd.plist
driver/mars.driver/
```

Each `manifest.json` file entry carries `path`, `sha256`, and `codesign_id`
(the `codesign` `Authority=` string read back from the signed artifact; never
invented). Embed the tarball as an app resource (Tauri: `resources` in
`tauri.conf.json`; native: `Contents/Resources`).

## Install flow and elevation timing

The flow is split so that **all validation happens before elevation** and the
privileged part is a single idempotent script:

1. **Unpack + verify (unprivileged).** `runtime::unpack_package` then
   `runtime::verify_package`: per-file SHA-256, `codesign --verify --strict`
   against the manifest `codesign_id`, `xcrun stapler validate` on the driver
   bundle, IPC protocol compatibility, and the `min_macos` floor. Never prompt
   the user for admin rights before this step succeeds — elevation must only
   ever execute a package you have already authenticated.
2. **Drain (unprivileged).** `runtime::bootout_daemon()` stops the running
   daemon so the privileged copy never races an active render loop.
3. **Privileged script.** `runtime::render_privileged_install_script(package_dir)`
   returns a root-only script that installs `/usr/local/bin/{mars,marsd}`,
   replaces `/Library/Audio/Plug-Ins/HAL/mars.driver`, and reloads
   `coreaudiod` (`killall -9 coreaudiod`, falling back to
   `launchctl kickstart -k system/com.apple.audio.coreaudiod`). Run it through
   your own elevation flow: an SMJobBless/SMAppService helper, an
   `AuthorizationExecuteWithPrivileges`-style bridge, or
   `osascript -e 'do shell script ... with administrator privileges'`.
   The script is idempotent — re-running after a partial failure is safe.
4. **User part (unprivileged).** `runtime::install_user_components` creates
   `~/Library/{Logs,Caches,Application Support}/mars`, renders the LaunchAgent
   plist into `~/Library/LaunchAgents/com.mars.marsd.plist`, bootstraps it via
   `launchctl bootstrap/enable/kickstart` in the user's `gui/<uid>` domain,
   and writes the install receipt
   (`~/Library/Application Support/mars/runtime-manifest.json`) that
   `runtime_status` uses for `installed_version`.

The LaunchAgent has `KeepAlive=true`, so if the user part runs before the
privileged part (the CLI does this when `--privileged-exec` is not passed),
launchd starts the daemon automatically once the binaries appear.

CLI equivalent:

```bash
mars runtime install --package mars-runtime-0.2.0.tar.gz --json            # prints the script path
mars runtime install --package mars-runtime-0.2.0.tar.gz --privileged-exec # runs it via sudo
```

`--allow-unsigned` skips signature and staple verification. It exists for
local development packages only (e.g. ad-hoc signed builds on SIP-disabled
machines); a shipping app must never set it.

## Status state machine and health polling

`runtime::runtime_status(&layout, &StatusOptions::default())` (or
`mars runtime status --json`) is read-only, never auto-launches the daemon,
and bounds every check with an explicit timeout:

| `state`                 | Meaning                                                            | Typical app reaction              |
| ----------------------- | ------------------------------------------------------------------ | --------------------------------- |
| `missing`               | binaries or driver bundle absent                                   | offer install                     |
| `installed_not_running` | files present, daemon not answering pings                          | kickstart agent, then re-poll     |
| `healthy`               | daemon responding, versions agree                                  | normal operation                  |
| `stale`                 | daemon responding but older than the installed files/receipt       | restart daemon (`kickstart -k`)   |
| `incompatible`          | IPC protocol mismatch or driver/daemon major version mismatch      | run update with a matching package |

Post-install health polling: poll `runtime_status` once per second with a
~30 s overall budget. Expected transitions after a successful install are
`missing → installed_not_running → healthy`. `installed_not_running` is normal
for several seconds while coreaudiod restarts and launchd spawns `marsd`
(KeepAlive rethrottles respawns at ~10 s, so do not give up before the
budget expires). End states to surface as errors: still `missing` (privileged
script never ran), `incompatible` (wrong package), or `stale` that survives a
`launchctl kickstart -k gui/<uid>/com.mars.marsd`.

## Upgrade (drain order)

`mars runtime update` enforces this order; replicate it if you drive the SDK
directly:

1. verify the new package (unprivileged),
2. compare versions — downgrades are refused (`version_downgrade`), equal
   versions are a no-op (`already_current`),
3. `bootout_daemon()` — drain the old daemon **before** any file is replaced,
4. privileged script: copy binaries + driver, reload coreaudiod,
5. `install_user_components` — re-bootstrap and kickstart the LaunchAgent,
   write the new receipt,
6. poll `runtime_status` until `healthy` reports the new `daemon_version`.

Skipping step 3 leaves the old daemon running against new files; status will
report `stale` until a kickstart. The version-compatibility gate in step 2 is
what keeps the driver and daemon upgrade atomic from the client's perspective
(both ship in one package, and `incompatible` is reported if they ever
diverge).

## Uninstall

`mars runtime uninstall` (or `uninstall_user_components` + the script from
`render_privileged_uninstall_script`) boots the agent out, removes the plist,
receipt, and `~/Library/Caches/mars`, and removes the binaries/driver in the
privileged part. All steps are idempotent. Note: orphaned POSIX shared-memory
rings named `mars.*` (left behind only if a process crashed mid-stream)
cannot be enumerated from user space; they are released when their owning
processes exit, or at reboot.

## Error codes

All failures carry a stable machine code (`RuntimeError::code()`, or the
`code` field of the CLI's `--json` error object): `manifest_missing`,
`manifest_invalid`, `unsafe_path`, `file_missing`, `sha256_mismatch`,
`unsigned_package`, `signature_invalid`, `signer_mismatch`, `staple_invalid`,
`protocol_unsupported`, `macos_too_old`, `version_downgrade`, `not_installed`,
`command_failed`, `command_timed_out`, `io_error`, `home_unavailable`.

## Doctor and the enumeration deadline

`mars doctor` (and `MarsClient::doctor()`) is safe to use as a dry-run health
check from an installer UI: the daemon runs CoreAudio enumeration on a worker
thread with a 3-second deadline. If coreaudiod is mid-restart and enumeration
stalls, the report still returns, with a note beginning with
`enumeration_timed_out:` and conservative defaults (driver reported not
loaded/compatible). Treat that marker as "retry shortly", not as a failed
install.

## TCC note (microphone permission)

The MARS virtual input is a CoreAudio input device. When **your** app opens
it (directly or through the capture pipeline), macOS attributes the access to
your app and shows the standard microphone permission prompt — installing
MARS does not pre-authorize anything. Your app therefore needs its own
`NSMicrophoneUsageDescription` in `Info.plist` (Tauri:
`bundle.macOS.usageDescriptions` / an Info.plist patch), and you should
expect a TCC prompt the first time you read audio from the MARS device.
`mars doctor`'s `microphone_permission_ok` reflects the daemon's TCC state,
not your app's.

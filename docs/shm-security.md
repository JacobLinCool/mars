# Shared-memory ring security model

MARS moves audio between processes through POSIX shared-memory ring buffers
(`mars-hal::shm_backend`, protocol v2). This document defines who may create,
read, and write a ring, and why the design is safe on a multi-user macOS
system.

## The cross-user problem

Ring producers and consumers run as **different users**:

| Role | Process | User |
| --- | --- | --- |
| Virtual output consumer / virtual input producer (render) | `marsd` | logged-in user |
| HAL plug-in (writes vout rings, reads vin rings) | `coreaudiod` / DriverHelper | `_coreaudiod` |
| App-owned virtual input producer (issue #35/#40) | downstream app | logged-in user |

POSIX SHM on macOS has **no ACLs** — only owner/group/other permission bits —
and `fchmod` on SHM descriptors fails with `EINVAL`, so the permission bits
must be correct at `shm_open` time. Rings created `0o600` pass same-user unit
tests but are unreadable from coreaudiod on the real audio path (issue #39).

## Design

Three cooperating mechanisms:

1. **Permission bits**: rings are created world-rw (`0o666` by default,
   override with `MARS_SHM_MODE`, octal). The creating call clears the
   process umask for the duration of the single `shm_open` syscall (under a
   process-local lock) because macOS offers no post-creation chmod for SHM
   objects.

2. **Capability-token naming**: world-rw objects are gated by *name
   unguessability*. Each device uid gets a persistent random 64-bit token
   (`mars_shm::ring_token_for`), stored 0600 in
   `~/Library/Application Support/mars/ring_tokens.json`. The logical ring
   name is `mars.{vout|vin}.<uid>.<token>`. The token travels only over
   trusted channels:
   - to the HAL: inside the DesiredState JSON on the CoreAudio property
     channel (`HalDevice.ring_token`);
   - to SDK clients: over the per-user Unix socket (itself 0600).

3. **Digest object names**: macOS limits SHM names to 31 bytes
   (`PSHMNAMLEN`), so the POSIX object name is a fixed-length FNV-1a digest
   of the logical name: `/mars.<16 hex>`. Both sides derive it from the same
   logical name. A process that never learned the token cannot construct the
   object name; brute-forcing the 64-bit token space through `shm_open`
   probing is infeasible. (The digest scheme also fixes a latent v1 bug:
   logical names longer than 31 bytes failed `shm_open` with `ENAMETOOLONG`
   and the failure was swallowed by `.ok()` on the render path.)

## Ownership and lifecycle

- **Creation**: whichever side starts first creates the object (`O_CREAT |
  O_EXCL`, then validation); the other side opens and validates magic,
  version, sample rate, channels, and capacity, recreating the object when
  incompatible (covers protocol-v1 leftovers).
- **Writes**: strict field ownership per ring protocol v2 — the producer owns
  `write_idx`/`overrun_count`, the consumer owns `underrun_count`, and
  `read_idx` advances only by compare-exchange from either side.
- **Cleanup**: the daemon unlinks rings on `clear`/device removal
  (`RingRegistry::remove`, `remove_namespace("mars.")` keyed by *logical*
  names tracked in-process); the HAL unlinks on device destruction. POSIX SHM
  objects do not survive reboot, so orphans from crashed processes are
  bounded by uptime.
- **No silent fallback**: ring open failures in the HAL fail `StartIO`
  (`kAudioHardwareIllegalOperationError`); daemon-side failures surface
  through status/doctor rather than silently rendering silence.

## Residual risks

- Any process belonging to a user that learned a token (e.g. reading the
  0600 token store as that user) can write the ring — same trust boundary as
  the IPC socket itself.
- A malicious *root* process can do anything; root is out of scope.
- Token rotation only happens when the token store is deleted; rotating on
  every apply would churn HAL devices. Delete
  `~/Library/Application Support/mars/ring_tokens.json` and re-apply to force
  rotation.

## Acceptance

- Unit: `created_rings_are_cross_user_readable_and_writable` (mode bits),
  `posix_names_fit_macos_pshmnamlen_for_any_logical_name` (digest naming),
  `ring_tokens_are_stable_and_unguessable` (token store).
- Cross-user integration: `scripts/acceptance/shm-cross-user.sh` (requires
  sudo; see header comments).
- Real-HAL verification is part of the issue #41 acceptance harness:
  `mars doctor` must report the HAL-side ring attach state after a real
  apply.

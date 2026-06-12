# HAL realtime invariants

`plugin_do_io_operation` and `plugin_get_zero_time_stamp` run on
coreaudiod's realtime IO thread. Downstream virtual-microphone apps depend
on this path being predictable in meeting/recording/browser clients
(issue #38). The following invariants hold and must be preserved by any
change to `crates/mars-hal`:

## Invariants

1. **No heap allocation.** The callback performs zero `malloc`/`realloc`/
   `free` after `StartIO`. Enforced by
   `do_io_operation_has_zero_heap_allocation_steady_state`
   (`plugin_tests.rs`), which runs 64 cycles (including ring wraps) under a
   counting allocator.
2. **No blocking locks.** Device state is resolved through the lock-free
   `RT_DEVICES` snapshot (`arc_swap::ArcSwap`); per-device mutable state
   (`channels`, `is_input`, `volume_scalar`, `sample_time_frames`,
   `zero_ts_seed`) is atomics inside `RtDeviceState`; runtime xrun counters
   are static atomics (`RUNTIME_STATS`). The only lock on the path is the
   ring handle `try_lock` — non-blocking by construction, with
   drop-on-contention (output) / silence-on-contention (input) fallbacks.
3. **No name construction or registry lookups.** The ring handle is cached
   into `RtDeviceState.ring` by `plugin_start_io` (the only point where the
   ring is guaranteed to exist) and invalidated when the device shape or
   ring token changes. The callback never builds strings, hashes names, or
   touches `global_registry()`.
4. **No syscalls beyond the clock.** `SystemTime::now` (commpage read) and
   `mach_absolute_time` are the only kernel-adjacent calls. `shm_open`/
   `mmap`/`shm_unlink` happen exclusively on non-RT paths (`StartIO`,
   configuration sync, destruction).
5. **Silence on underrun, drop on contention/absence.** A missing producer,
   an empty ring, a held ring lock, or an uncached ring all yield zeroed
   input buffers (or dropped output frames) and bump the atomic xrun
   counters — never a block, never an error into the HAL.
6. **Cross-process header discipline.** Ring header access follows the
   protocol v2 ownership rules (see `shm_backend.rs` module docs and
   `docs/shm-security.md`): field-scoped atomics, Release-publish of
   `write_idx` after frame data, compare-exchange advances of `read_idx`.
   The callback never re-reads the shared header for stat deltas — the
   transfer result carries this call's deltas (`RingTransfer`).

## Non-RT paths with shared state

`sync_object_registry`, property handlers, and `StartIO/StopIO` may take
the `object_registry`/`DRIVER_STATE` mutexes and perform syscalls — they
publish RT-visible changes only through `publish_rt_snapshot` (ArcSwap
store) and per-device atomics. Long work under those mutexes is still
discouraged (it delays other non-RT paths), but it can no longer stall the
realtime thread.

## Review checklist for changes touching this path

- [ ] No new `.lock()` (blocking) on anything reachable from
      `plugin_do_io_operation` / `plugin_get_zero_time_stamp`
- [ ] No `String`/`Vec`/`format!`/`Box` construction in the callback
- [ ] New per-device state goes into `RtDeviceState` as atomics (or a new
      snapshot publication), not into `DeviceObjectInfo` reads
- [ ] `do_io_operation_has_zero_heap_allocation_steady_state` still passes
- [ ] Ring protocol changes preserve the v2 ownership rules and bump
      `RING_VERSION` when the layout moves

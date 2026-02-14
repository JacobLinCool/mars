# POSIX SHM Design (Step 2)

`mars-shm` is the **safe facade**.
`mars-hal::shm_backend` is the **actual mmap + POSIX SHM backend**.

## Naming

- Public stream names: `mars.vout.<uid>` / `mars.vin.<uid>`
- POSIX object names: `/mars.vout.<uid>` / `/mars.vin.<uid>` (sanitized)

## Memory layout

- Header (fixed 52 bytes, little-endian):
  - `magic`
  - `version`
  - `sample_rate`
  - `channels`
  - `capacity_frames`
  - `write_idx`
  - `read_idx`
  - `overrun_count`
  - `underrun_count`
- Data region: interleaved `f32` frames

## Behavior

- Overrun: overwrite oldest frame (`read_idx += 1`) and increment `overrun_count`
- Underrun: fill output with silence and increment `underrun_count`

## Safety boundary

`unsafe` is centralized in `mars-hal::shm_backend` only.
`mars-shm` uses `#![forbid(unsafe_code)]` and only re-exports safe APIs.

Unsafe sites (all in `mars-hal::shm_backend`):

1. `mmap` call
2. `munmap` call
3. `from_raw_parts` / `from_raw_parts_mut` conversion
4. `Send` impl for mapped pointer holder

Each site includes explicit `SAFETY` comments.

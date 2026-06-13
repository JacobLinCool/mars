# Real-HAL virtual input acceptance (issue #41)

Downstream adoption needs proof that the full path works in the real macOS
audio stack — not just daemon-internal routing. This document covers the
automated harness and the manual runbook.

## Automated harness

```bash
scripts/acceptance/hal-virtual-input.sh
```

What it exercises:

1. preflight — signed `mars.driver` present in `/Library/Audio/Plug-Ins/HAL`,
   `marsd` reachable (exits `2` with instructions when unmet: these are the
   manual-setup prerequisites that signing/permissions impose)
2. an external producer (`mars-sdk` example `virtual_mic_producer`) ensures
   an app-owned virtual input (`producer: external_app`) and streams a
   440 Hz sine through `LiveWriter`
3. `mars status --json` must report the producer `active`
4. a CoreAudio client (`virtual_mic_reader`, cpal-based) captures 2 s from
   the virtual input and verifies the tone with a Goertzel filter — proving
   ring data crossed the real HAL
5. the producer is killed; a second capture must read silence
6. cleanup; on any failure the harness saves `status.json`, `doctor.txt`,
   and the last 5 minutes of mars system logs to a temp directory

Exit codes: `0` pass, `1` fail (diagnostics captured), `2` manual setup
required.

## Manual runbook

Run these once per release on real hardware. Each step states the expected
observation.

### QuickTime capture check

1. `scripts/acceptance/hal-virtual-input.sh` preflight must pass.
2. Start the producer:
   `cargo run -p mars-sdk --example virtual_mic_producer -- 120`
3. QuickTime Player → File → New Audio Recording → source selector →
   choose **MARS Acceptance Mic**.
4. Record 10 s. Expected: a clean 440 Hz tone in the recording, no
   dropouts; the input level meter moves steadily.
5. Kill the producer mid-recording. Expected: the recording continues with
   silence — no error dialog, no device disappearance.

### Zoom / Google Meet device-selection check

1. With the producer running, open Zoom → Settings → Audio (or
   meet.google.com → Settings → Audio).
2. Expected: **MARS Acceptance Mic** appears in the microphone list; the
   input level indicator shows activity; selecting it and joining a test
   meeting carries the tone.
3. TCC note: the browser/Zoom needs microphone permission — the prompt
   names the app, not MARS (the virtual device is system-level).

### Device UID stability across reinstall

1. Select MARS Acceptance Mic in Zoom and leave it selected.
2. Upgrade or reinstall the runtime (`mars runtime update`, or
   uninstall + install).
3. Re-ensure the device with the same `uid`
   (`com.mars.acceptance.mic`).
4. Expected: Zoom still has the device selected (CoreAudio matches by
   UID); no re-selection needed.

### Sample-rate lock (issue #48)

1. With the device applied, open Audio MIDI Setup.
2. Expected: the format selector for MARS Acceptance Mic offers exactly
   one sample rate (48,000 Hz); attempting to set another rate via
   `AudioObjectSetPropertyData` fails with
   `kAudioHardwareIllegalOperationError` (covered by the
   `nominal_sample_rate_is_locked_to_the_applied_rate` unit test).

## Failure triage

| Symptom | First checks |
| --- | --- |
| Reader exits 2 (device not found) | `mars doctor` driver section; was the profile applied? coreaudiod restarted after install? |
| Tone check fails but producer is `active` | `mars status --json` → `virtual_input_producers[].underrun_count`; ring attach (#39): run `scripts/acceptance/shm-cross-user.sh` |
| Silence check fails | producer process actually dead? `producer_generation` should be even (detached) in status |
| Producer `stale` in status | the producer loop is blocked or its process was suspended; check `producer.log` |

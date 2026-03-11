# MARS

MARS (macOS Audio Router Service) is an audio routing system for macOS 15+.

## What is included

- `mars` CLI with commands: `create`, `open`, `apply`, `clear`, `validate`, `plan`, `status`, `devices`, `processes`, `test`, `logs`, `doctor`
- `marsd` daemon with declarative apply transaction and rollback semantics
- `mars-sdk` Rust SDK crate for building external apps/tools on the typed MARS API
- `mars-hal` AudioServerPlugIn driver crate and `mars.driver` bundle scaffold
- Profile schema `version: 2` only (`version: 1` is rejected)
- Process/system capture tap model (`captures.process_taps` and `captures.system_taps`)
- Built-in file sinks (`sinks.files`) for WAV/CAF recording
- AUv2/AUv3 processor hosting through isolated `mars-plugin-host`
- Shared profile schema, graph validator, ring-buffer model, and realtime engine core

## Current limitations

- `sinks.streams` is an extensible descriptor model, but stream sink runtime is not implemented yet.
- When a stream sink is configured, runtime status/doctor surfaces it as failed with `last_error` details.

## Architecture

```mermaid
graph TB
    subgraph UserSpace ["User Space"]
        App1["App (Browser)"]
        App2["App (Music)"]
        App3["App (Zoom)"]
        YAML["Profile YAML"]
        CLI["mars CLI"]
    end

    subgraph Daemon ["marsd (Daemon Process)"]
        direction TB
        IPC["IPC Server<br/>Unix Socket"]
        Control["Control Thread"]
        ProfileLoader["Profile Loader<br/>mars-profile"]
        GraphValidator["Graph Validator<br/>mars-graph"]
        Planner["Planner<br/>diff & plan"]
        HalClient["HAL Client<br/>mars-hal-client"]
        ExtIO["External I/O<br/>mars-coreaudio"]
        ShmManager["SHM Manager<br/>mars-shm"]
        RenderRT["Render Runtime<br/>Realtime Audio Thread"]
        Engine["Engine<br/>mars-engine"]
    end

    subgraph CoreAudioD ["coreaudiod (System Process)"]
        direction TB
        Driver["mars.driver<br/>AudioServerPlugIn<br/>mars-hal"]
        VOutDev["Virtual Output Devices<br/>(App writes here)"]
        VInDev["Virtual Input Devices<br/>(App reads here)"]
    end

    subgraph ExternalHW ["External Hardware"]
        Mic["Microphone"]
        Speaker["Speakers / Headphones"]
    end

    subgraph SharedMem ["POSIX Shared Memory"]
        ShmVOut["mars.vout.&lt;uid&gt;<br/>Ring Buffer"]
        ShmVIn["mars.vin.&lt;uid&gt;<br/>Ring Buffer"]
    end

    %% Control Plane
    YAML -->|"parse & validate"| CLI
    CLI <-->|"JSONL over Unix Socket"| IPC
    IPC <--> Control
    Control --> ProfileLoader
    ProfileLoader --> GraphValidator
    GraphValidator --> Planner
    Control --> HalClient
    HalClient <-->|"CoreAudio Properties<br/>(DesiredState / AppliedState)"| Driver

    %% Data Plane — Virtual Output path
    App1 -->|"audio output"| VOutDev
    App2 -->|"audio output"| VOutDev
    VOutDev -->|"WriteMix"| Driver
    Driver -->|"write samples"| ShmVOut
    ShmVOut -->|"read samples"| RenderRT

    %% Data Plane — Engine
    RenderRT --> Engine
    Engine -->|"gain / pan / delay<br/>mix / limiter"| RenderRT

    %% Data Plane — Virtual Input path
    RenderRT -->|"write samples"| ShmVIn
    ShmVIn -->|"ReadInput"| Driver
    Driver --> VInDev
    VInDev -->|"audio input"| App3

    %% Data Plane — External I/O
    Mic -->|"capture"| ExtIO
    ExtIO -->|"input samples"| RenderRT
    RenderRT -->|"output samples"| ExtIO
    ExtIO -->|"playback"| Speaker

    %% SHM management
    Control --> ShmManager
    ShmManager --> SharedMem

    class CLI,IPC,Control,ProfileLoader,GraphValidator,Planner,HalClient,ExtIO,ShmManager,RenderRT,Engine process
    class Driver,VOutDev,VInDev driver
    class ShmVOut,ShmVIn,SharedMem shm
    class Mic,Speaker hw
    class App1,App2,App3,YAML user
```

### Crate Dependency Graph

```mermaid
graph BT
    types["mars-types<br/><i>shared types & constants</i>"]
    graph_["mars-graph<br/><i>routing graph & validation</i>"]
    profile["mars-profile<br/><i>YAML parsing & schema</i>"]
    engine["mars-engine<br/><i>realtime audio rendering</i>"]
    coreaudio["mars-coreaudio<br/><i>external device I/O</i>"]
    hal["mars-hal<br/><i>AudioServerPlugIn driver</i>"]
    halclient["mars-hal-client<br/><i>safe driver API</i>"]
    shm["mars-shm<br/><i>ring buffer facade</i>"]
    ipc["mars-ipc<br/><i>Unix socket protocol</i>"]
    sdk["mars-sdk<br/><i>public app SDK</i>"]
    daemon["mars-daemon<br/><i>marsd orchestrator</i>"]
    cli["mars-cli<br/><i>user interface</i>"]

    graph_ --> types
    profile --> types
    profile --> graph_
    engine --> types
    engine --> graph_
    coreaudio --> types
    shm --> hal
    halclient --> hal
    ipc --> types
    sdk --> types
    sdk --> ipc
    daemon --> profile
    daemon --> engine
    daemon --> coreaudio
    daemon --> shm
    daemon --> halclient
    daemon --> ipc
    cli --> types
    cli --> profile
    cli --> ipc

    class types foundation
    class graph_,profile,engine,ipc,sdk core
    class hal,halclient,shm,coreaudio platform
    class daemon,cli app
```

### Audio Routing Example

```mermaid
flowchart LR
    subgraph Sources
        vout1["Bus: Browser<br/>(Virtual Output)"]
        vout2["Bus: Music<br/>(Virtual Output)"]
        mic["Microphone<br/>(External Input)"]
    end

    subgraph Processing
        bus["merge-bus<br/>(Bus Node)<br/>gain / mix / limiter"]
    end

    subgraph Sinks
        vin["Mix: Main<br/>(Virtual Input)"]
        spk["Speakers<br/>(External Output)"]
    end

    vout1 -->|"gain: -6 dB"| bus
    vout2 -->|"gain: 0 dB"| bus
    mic -->|"gain: -6 dB"| bus
    bus --> vin
    bus -->|"gain: -3 dB"| spk
```

## Build

```bash
cargo build
cargo test
```

Engine robustness and perf checks:

```bash
cargo test -p mars-engine --test soak
cargo test -p mars-engine --release --test perf_gate -- --ignored
cargo bench -p mars-engine --bench engine -- engine/render_multisource_multioutput
cargo bench -p mars-daemon --bench daemon_ipc_shm
```

Benchmark gate policy, CI job mapping, and local reproduction commands:
`docs/performance-gates.md`.

## Getting Started

See the full setup and first-run guide: `docs/getting-started.md`.

Quick install:

```bash
./scripts/install.sh
```

Run as your normal user (do not prefix with `sudo`).

If you need local-only insecure signing for development, opt in explicitly:

```bash
MARS_ALLOW_INSECURE_SIGNING=1 ./scripts/install.sh
```

Quick health check:

```bash
mars doctor
```

If logs report `Mars driver plugin not found in loaded CoreAudio plugins`, run:

```bash
sudo killall -9 coreaudiod
```

## Usage

The default template includes process/system taps and a stream sink descriptor. Before `mars apply`, update or remove those entries so they match your host:
- use `mars processes --json` to select real process selectors (PID or bundle id)
- remove `captures` or `sinks.streams` entries you do not need

```bash
mars create demo
mars open demo
mars processes --json
mars validate demo
mars plan demo
mars apply demo
mars status --json
mars processes --json
mars doctor
mars test
mars test --route
mars clear
```

`mars test` measures internal MARS data-plane latency only.
`mars test --route` verifies the microphone-to-speaker and microphone-to-virtual-capture route.

## SDK

For third-party app/tool development, use the Rust SDK:

- docs: `docs/sdk.md`
- example: `cargo run -p mars-sdk --example status`

## Uninstall

```bash
./scripts/uninstall.sh
```

Run as your normal user (do not prefix with `sudo`).

For more operational commands and runtime paths, see `docs/operator-guide.md`.

## Development run

```bash
cargo run -p mars-daemon --bin marsd -- --serve
```

`marsd` requires a real loaded `mars.driver` bundle.

## Logs

```bash
mars logs
./scripts/logs.sh
```

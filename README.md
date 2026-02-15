# MARS

MARS (macOS Audio Router Service) is an audio routing system for macOS.

## What is included

- `mars` CLI with commands: `create`, `open`, `apply`, `clear`, `validate`, `plan`, `status`, `devices`, `logs`, `doctor`
- `marsd` daemon with declarative apply transaction and rollback semantics
- `mars-hal` AudioServerPlugIn driver crate and `mars.driver` bundle scaffold
- Shared profile schema, graph validator, ring-buffer model, and realtime engine core

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
    class graph_,profile,engine,ipc core
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

## Getting Started

See the full setup and first-run guide: `docs/getting-started.md`.

Quick install:

```bash
./scripts/install.sh
```

Run as your normal user (do not prefix with `sudo`).

For local-only development on SIP-disabled systems, you can explicitly opt in to insecure signing:

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

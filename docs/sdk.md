# MARS SDK (Rust)

`mars-sdk` is the public Rust integration layer for building apps and tools on top of `marsd`.

## Contract scope

`mars-sdk` wraps the public daemon contract:

- Protocol transport and envelopes from `mars-ipc`
- Request/response types from `mars-types`
- Typed async operations through `MarsClient`

## Add dependency

```toml
[dependencies]
mars-sdk = { path = "crates/mars-sdk" }
```

Feature notes:

- default feature `default-socket-path` enables `MarsClient::new_default()` and uses `dirs` to resolve `~/<cache>/mars/marsd.sock`.
- disable defaults if your app always provides an explicit socket path:

```toml
[dependencies]
mars-sdk = { path = "crates/mars-sdk", default-features = false }
```

## Quickstart

```rust
use mars_sdk::MarsClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = MarsClient::new_default(MarsClient::default_timeout())?;

    client.ping().await?;
    let status = client.status().await?;

    println!("running={} profile={:?}", status.running, status.current_profile);
    Ok(())
}
```

## API summary

- `MarsClient::new(socket_path, timeout)`
- `MarsClient::new_default(timeout)`
- `MarsClient::ping()`
- `MarsClient::validate()/validate_profile()`
- `MarsClient::plan()/plan_profile()`
- `MarsClient::apply()/apply_profile()`
- `MarsClient::clear()`
- `MarsClient::status()`
- `MarsClient::devices()`
- `MarsClient::processes()`
- `MarsClient::logs()/logs_once()`
- `MarsClient::doctor()`
- `MarsClient::ensure_virtual_input()/remove_virtual_input()/virtual_input_status()`

## Virtual microphone (app-owned producer)

Downstream apps can own a virtual microphone end to end: MARS stages the
HAL device and reports producer health, while the app is the sole audio
producer. Devices are app-scoped declarative leases — persisted across
daemon restarts, applied atomically, and conflict-checked against the user
profile and other apps.

```rust
use mars_sdk::{AppVirtualInput, MarsClient, ProducerKind};

let client = MarsClient::new_default(MarsClient::default_timeout())?;
let mic = client
    .ensure_virtual_input(AppVirtualInput {
        app_id: "com.example.virtual-mic-app".into(),
        id: "primary-mic".into(),
        name: "Virtual Mic".into(),
        uid: "com.example.virtual-mic-app.primary-mic".into(),
        sample_rate: 48_000, // locked; see issue #48
        channels: 1,
        producer: ProducerKind::ExternalApp,
    })
    .await?;

let mut writer = mic.open_live_writer()?; // RT-safe from your audio callback
writer.write_f32_interleaved_live(&frames)?;
writer.clear_unread();     // drop backlog on mode changes
writer.flush_silence()?;   // smooth decay before shutdown
drop(writer);              // detaches; `mars status` shows producer absent
```

Producer health (`absent` / `active` / `stale` / `underrunning`) is visible
in `mars status --json` under `virtual_input_producers` and via
`client.virtual_input_status(app_id, id)`.

## Runtime install management

The `mars_sdk::runtime` module manages the installed runtime itself
(package verification, install/update/uninstall, and a read-only
`runtime_status()` state machine). See
[installer-embedding.md](installer-embedding.md) for the full embedding
guide.

## Example

Run the bundled example:

```bash
cargo run -p mars-sdk --example status
```

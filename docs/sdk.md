# MARS SDKs

MARS provides Rust, TypeScript, and Python integration layers for building
apps and tools on top of `marsd`. This page documents Rust. See
[`language-sdks.md`](language-sdks.md) for TypeScript and Python.

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
- `MarsClient::set_virtual_inputs()/get_virtual_inputs()/virtual_input_status()`

## Virtual microphone (app-owned producer)

Downstream apps declare their complete set of virtual microphones: MARS
persists the app-scoped intent, stages the HAL devices, and reports producer
health while the app remains the sole audio producer. Replacing the set is
atomic; passing an empty vector removes every input owned by that app without
affecting other apps.

```rust
use mars_sdk::{AppVirtualInputSpec, MarsClient};

let client = MarsClient::new_default(MarsClient::default_timeout())?;
let outcome = client
    .set_virtual_inputs("com.example.virtual-mic-app", vec![AppVirtualInputSpec {
        id: "primary-mic".into(),
        name: "Virtual Mic".into(),
        uid: "com.example.virtual-mic-app.primary-mic".into(),
        sample_rate: 48_000,
        channels: 1,
    }])
    .await?;
let mic = &outcome.virtual_mics[0];

let mut writer = mic.open_live_writer()?; // RT-safe from your audio callback
writer.write_f32_interleaved_live(&frames)?;
writer.clear_unread();     // drop backlog on mode changes
writer.flush_silence()?;   // smooth decay before shutdown
drop(writer);              // detaches; `mars status` shows producer absent
```

The declaration persists across daemon restarts. `mars clear` clears the user
base profile but leaves app-owned inputs intact. Producer health (`absent` /
`active` / `stale` / `underrunning`) is visible in `mars status --json` under
`virtual_input_producers` and via `client.virtual_input_status(app_id, id)`.

App-produced inputs must be declared through this SDK API. User profile YAML
cannot set `producer: external_app` because it has no owning `app_id`.

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

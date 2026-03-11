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

## Example

Run the bundled example:

```bash
cargo run -p mars-sdk --example status
```

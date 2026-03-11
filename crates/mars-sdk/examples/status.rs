use mars_sdk::MarsClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = MarsClient::new_default(MarsClient::default_timeout())?;
    client.ping().await?;

    let status = client.status().await?;
    println!(
        "running={} profile={:?}",
        status.running, status.current_profile
    );

    Ok(())
}

/// Purge all messages from the MATCH JetStream stream.
/// Usage: cargo run -p routers_realtime --example nats_purge
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());

    let nc = async_nats::connect(&nats_url).await?;
    let js = async_nats::jetstream::new(nc);

    match js.get_stream("MATCH").await {
        Ok(mut stream) => {
            let info = stream.info().await?;
            let before = info.state.messages;
            stream.purge().await?;
            println!("Purged {before} messages from MATCH stream.");
        }
        Err(_) => {
            println!("MATCH stream does not exist — nothing to purge.");
        }
    }

    Ok(())
}

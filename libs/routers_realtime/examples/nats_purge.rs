/// Purge or delete EVENTS JetStream stream and delete the legacy MATCH stream.
/// Usage:
///   cargo run -p routers_realtime --example nats_purge              # purge messages only
///   cargo run -p routers_realtime --example nats_purge -- --delete  # delete stream entirely (forces subject migration on next start)
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());

    let delete_events = std::env::args().any(|a| a == "--delete");

    let nc = async_nats::connect(&nats_url).await?;
    let js = async_nats::jetstream::new(nc);

    if delete_events {
        match js.delete_stream("EVENTS").await {
            Ok(_) => println!("Deleted EVENTS stream (will be recreated with correct subjects on next orchestrator start)."),
            Err(_) => println!("EVENTS stream does not exist — nothing to delete."),
        }
    } else {
        match js.get_stream("EVENTS").await {
            Ok(mut stream) => {
                let info = stream.info().await?;
                let before = info.state.messages;
                stream.purge().await?;
                println!("Purged {before} messages from EVENTS stream.");
            }
            Err(_) => {
                println!("EVENTS stream does not exist — nothing to purge.");
            }
        }
    }

    // MATCH stream is legacy — matching now uses core NATS publish/subscribe.
    // Delete it so it does not intercept match.* subjects.
    match js.delete_stream("MATCH").await {
        Ok(_) => println!("Deleted legacy MATCH stream."),
        Err(_) => println!("MATCH stream does not exist — nothing to delete."),
    }

    Ok(())
}

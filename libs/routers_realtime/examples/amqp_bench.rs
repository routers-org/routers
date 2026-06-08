/// Pure AMQP receive-and-ack benchmark — no Valkey, no NATS, no JSON parsing.
/// Measures the raw ceiling for how fast a single consumer can drain a queue
/// through the current connection (port-forward or direct).
///
/// Run after flooding the queue with `REPLAY_FLOOD=1 just replay`:
///   RABBITMQ_URL=... cargo run --release -p routers_realtime --example amqp_bench
use futures::StreamExt;
use routers_realtime::amqp::{Topic, TopicOpts};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let topic = Topic::new(TopicOpts::from_env()).await?;
    let mut stream = topic;

    let mut count = 0u64;
    let t0 = std::time::Instant::now();
    let mut last_report = t0;

    while let Some(delivery) = stream.next().await {
        let delivery = delivery?;
        delivery
            .ack(lapin::options::BasicAckOptions::default())
            .await?;
        count += 1;

        let now = std::time::Instant::now();
        let since_report = now.duration_since(last_report).as_secs_f64();
        if since_report >= 1.0 {
            let total_s = now.duration_since(t0).as_secs_f64();
            let rate = count as f64 / total_s;
            eprintln!(
                "[{:.1}s] consumed: {:>8}   rate: {:>8.0} evt/s",
                total_s, count, rate
            );
            last_report = now;
        }
    }

    let total_s = t0.elapsed().as_secs_f64();
    println!(
        "Done: {} events in {:.2}s = {:.0} evt/s",
        count,
        total_s,
        count as f64 / total_s
    );
    Ok(())
}

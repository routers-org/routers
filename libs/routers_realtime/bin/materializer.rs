//! The materialiser service.
//!
//! Tails the committed-output plane with a durable pull consumer and applies
//! every output to a [`ValkeySink`], persisting each before acknowledging. It
//! owns none of the orchestrator's state: the output plane defines the view.

use core::ops::RangeInclusive;
use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::AckKind;
use async_nats::jetstream::consumer::{AckPolicy, PullConsumer, pull};
use async_nats::jetstream::stream::Stream as JetStream;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::StreamExt;
use log::{info, warn};
use serde::de::DeserializeOwned;
use url::Url;

use routers_codec::osm::OsmEntryId;
use routers_network::Entry;
use routers_realtime::bus::Wire;
use routers_realtime::bus::adapter::{AckHandle, Delivery, Source};
use routers_realtime::lifecycle::Shutdown;
use routers_realtime::materializer::{ValkeySink, consumer};
use routers_realtime::partition::PARTITIONS;
use routers_realtime::protocol::ids::headers;
use routers_realtime::protocol::output::CommittedOutput;
use routers_realtime::topology;

/// The entry type the fleet solves against.
type E = OsmEntryId;

/// Parse an inclusive partition range: "start-end", or a single partition.
fn parse_partitions(spec: &str) -> Result<RangeInclusive<u64>, String> {
    let (start, end) = spec.split_once('-').unwrap_or((spec, spec));
    let start: u64 = start
        .trim()
        .parse()
        .map_err(|err| format!("bad start: {err}"))?;
    let end: u64 = end
        .trim()
        .parse()
        .map_err(|err| format!("bad end: {err}"))?;
    if start > end {
        return Err(format!("start {start} beyond end {end}"));
    }
    if end >= PARTITIONS {
        return Err(format!("partition {end} outside 0..{PARTITIONS}"));
    }
    Ok(start..=end)
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the NATS server.
    #[arg(short, env, long)]
    nats: Url,

    /// Valkey primaries, comma-separated. Order is irrelevant (rendezvous hash),
    /// but every process touching the served view must be given the same set.
    #[arg(short, env, long, value_delimiter = ',')]
    valkey: Vec<Url>,

    /// The partitions to materialise, as an inclusive range ("0-255"); omitted,
    /// the consumer tails the whole output plane.
    #[arg(short, env, long, value_parser = parse_partitions)]
    partitions: Option<RangeInclusive<u64>>,

    /// The durable consumer name. Replicas that share a name share the work;
    /// a distinct name replays the plane independently.
    #[arg(long, env, default_value = "materializer")]
    consumer_name: String,
}

/// A [`Source`] over a JetStream pull consumer.
struct JetStreamSource {
    messages: pull::Stream,
}

/// The acknowledgement handle for one pulled output.
struct JetStreamAck {
    message: async_nats::jetstream::Message,
    sequence: u64,
    deliveries: u32,
}

impl AckHandle for JetStreamAck {
    async fn ack(self) -> anyhow::Result<()> {
        self.message
            .ack()
            .await
            .map_err(|err| anyhow::anyhow!("acknowledge failed: {err}"))
    }

    async fn nak(self, delay: Option<Duration>) -> anyhow::Result<()> {
        self.message
            .ack_with(AckKind::Nak(delay))
            .await
            .map_err(|err| anyhow::anyhow!("negative acknowledge failed: {err}"))
    }

    fn sequence(&self) -> u64 {
        self.sequence
    }

    fn deliveries(&self) -> u32 {
        self.deliveries
    }
}

impl<T> Source<CommittedOutput<T>> for JetStreamSource
where
    T: Entry + DeserializeOwned,
{
    type Handle = JetStreamAck;

    async fn next(&mut self) -> Option<anyhow::Result<Delivery<CommittedOutput<T>, JetStreamAck>>> {
        let message = match self.messages.next().await? {
            Ok(message) => message,
            Err(err) => return Some(Err(anyhow::anyhow!("output pull error: {err}"))),
        };

        let (sequence, deliveries) = match message.info() {
            Ok(info) => (info.stream_sequence, info.delivered.max(0) as u32),
            Err(_) => (0, 1),
        };

        let item = match CommittedOutput::<T>::decode(&message.payload) {
            Ok(item) => item,
            Err(err) => {
                // Retire the poison so the broker does not redeliver it forever,
                // then surface the failure for the consumer to count.
                if let Err(ack_err) = message.ack().await {
                    warn!("could not ack poison output: {ack_err}");
                }
                return Some(Err(err.context("undecodable committed output")));
            }
        };

        let subject = message.subject.to_string();
        let msg_id = message
            .headers
            .as_ref()
            .and_then(|headers| headers::msg_id_of(headers).map(str::to_owned));

        Some(Ok(Delivery {
            item,
            handle: JetStreamAck {
                message,
                sequence,
                deliveries,
            },
            subject,
            msg_id,
            sent_at: None,
            redelivered: deliveries > 1,
        }))
    }
}

/// Build the durable pull consumer over the output plane, narrowed to
/// `partitions` when a range is given.
async fn build_consumer(
    stream: &JetStream,
    name: &str,
    partitions: Option<RangeInclusive<u64>>,
) -> anyhow::Result<PullConsumer> {
    match partitions {
        None => topology::output::output_consumer(stream, name)
            .await
            .context("could not create output consumer"),
        Some(range) => {
            let filter_subjects: Vec<String> =
                range.map(topology::output::output_subject).collect();
            stream
                .get_or_create_consumer(
                    name,
                    pull::Config {
                        durable_name: Some(name.to_owned()),
                        filter_subjects,
                        ack_policy: AckPolicy::Explicit,
                        ..Default::default()
                    },
                )
                .await
                .context("could not create ranged output consumer")
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _telemetry = routers_realtime::telemetry::init("routers-materializer");

    let args = Args::parse();
    info!("materializer started: {args:?}");

    let shutdown = Shutdown::from_signals();

    let nats_url = ServerAddr::from_url(args.nats).context("could not create NATS url")?;
    let client = ConnectOptions::new()
        .name("MaterializerService")
        .connect(nats_url)
        .await
        .context("could not connect to NATS")?;
    let context = async_nats::jetstream::new(client);

    let stream = topology::output::ensure_output_stream(
        &context,
        &topology::output::OutputConfig::default(),
    )
    .await?;
    let pull = build_consumer(&stream, &args.consumer_name, args.partitions).await?;
    let messages = pull
        .messages()
        .await
        .context("could not start the output pull")?;
    let source = JetStreamSource { messages };

    let sink = ValkeySink::connect(&args.valkey)
        .await
        .context("could not connect to valkey")?;

    let stats = run_materializer(source, sink, shutdown).await?;
    info!("materializer stopped: {stats:?}");
    Ok(())
}

/// Drive the consume loop for the concrete entry type.
async fn run_materializer<S>(
    source: JetStreamSource,
    sink: S,
    shutdown: Shutdown,
) -> anyhow::Result<consumer::Stats>
where
    S: routers_realtime::materializer::Sink<E>,
{
    consumer::run::<E, _, _>(source, sink, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_ranges_parse_and_validate() {
        assert_eq!(parse_partitions("0-255").unwrap(), 0..=255);
        assert_eq!(parse_partitions("7").unwrap(), 7..=7);
        assert!(parse_partitions("5-4").is_err());
        assert!(parse_partitions("0-1024").is_err());
    }

    #[test]
    fn args_default_the_consumer_name_and_partitions() {
        let args = Args::parse_from([
            "materializer",
            "--nats",
            "nats://localhost",
            "--valkey",
            "redis://localhost",
        ]);
        assert_eq!(args.consumer_name, "materializer");
        assert!(args.partitions.is_none());
        assert_eq!(args.valkey.len(), 1);
    }

    #[test]
    fn valkey_urls_split_on_commas() {
        let args = Args::parse_from([
            "materializer",
            "--nats",
            "nats://localhost",
            "--valkey",
            "redis://a:6379,redis://b:6379",
        ]);
        assert_eq!(args.valkey.len(), 2);
    }
}

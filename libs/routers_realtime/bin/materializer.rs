//! The materialiser service.
//!
//! Tails the committed-output plane with a durable pull consumer and applies
//! every output to a [`ValkeySink`], persisting each before acknowledging. It
//! owns none of the orchestrator's state: the output plane alone defines the
//! served view.

use core::ops::RangeInclusive;

use anyhow::Context as _;
use async_nats::jetstream::consumer::{AckPolicy, PullConsumer, pull};
use async_nats::jetstream::stream::Stream as JetStream;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use log::info;

use routers_codec::osm::OsmEntryId;
use routers_realtime::bus::adapter::Source;
use routers_realtime::bus::jetstream::JetStreamSource;
use routers_realtime::lifecycle::Shutdown;
use routers_realtime::materializer::{ValkeySink, consumer};
use routers_realtime::metrics::Metrics;
use routers_realtime::partition::PARTITIONS;
use routers_realtime::protocol::output::CommittedOutput;
use routers_realtime::secret::SecretUrl;
use routers_realtime::store::valkey::ValkeyEndpoint;
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
    nats: SecretUrl,

    /// Valkey primaries as stable-id=URL, comma-separated.
    #[arg(short, env, long, value_delimiter = ',')]
    valkey: Vec<ValkeyEndpoint>,

    /// The partitions to materialise, as an inclusive range ("0-255"); omitted,
    /// the consumer tails the whole output plane.
    #[arg(short, env, long, value_parser = parse_partitions)]
    partitions: Option<RangeInclusive<u64>>,

    /// The durable consumer name. Replicas that share a name share the work;
    /// a distinct name replays the plane independently.
    #[arg(long, env, default_value = "materializer")]
    consumer_name: String,
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
    // Built after telemetry so instruments bind to the installed meter.
    let metrics = Metrics::new();

    let args = Args::parse();
    info!("materializer started: {args:?}");

    let shutdown = Shutdown::from_signals();

    let nats_url =
        ServerAddr::from_url(args.nats.connection_url()).context("could not create NATS url")?;
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
    let source = JetStreamSource::<CommittedOutput<E>>::from_consumer(&pull)
        .await
        .context("could not start the output pull")?;

    let sink = ValkeySink::connect(&args.valkey)
        .await
        .context("could not connect to valkey")?;

    let stats = run_materializer(source, sink, shutdown, &metrics).await?;
    info!("materializer stopped: {stats:?}");
    Ok(())
}

/// Drive the consume loop for the concrete entry type.
async fn run_materializer<Src, S>(
    source: Src,
    sink: S,
    shutdown: Shutdown,
    metrics: &Metrics,
) -> anyhow::Result<consumer::Stats>
where
    Src: Source<CommittedOutput<E>>,
    S: routers_realtime::materializer::Sink<E>,
{
    consumer::run_with_metrics::<E, _, _>(source, sink, shutdown, metrics).await
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
            "primary=redis://localhost",
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
            "a=redis://a:6379,b=redis://b:6379",
        ]);
        assert_eq!(args.valkey.len(), 2);
    }
}

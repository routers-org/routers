use core::ops::RangeInclusive;
use std::time::Duration;

use routers_realtime::event::VehicleId;
use scc::HashCache;
use scc::hash_cache::Entry;

use routers_codec::osm::OsmEntryId;
use routers_realtime::{
    bus::{self, Wire},
    event::{MatchContext, MatchReply, MatchedEvent, Payload, RawEvent},
    partition::{self, PARTITIONS},
    store::RedisStore,
    topology,
};
use routers_transition::matcher::Trip;
use routers_transition::{Continuation, Origin};

use anyhow::{Context, Result, anyhow};
use async_nats::{ConnectOptions, ServerAddr, jetstream};
use clap::Parser;
use futures::StreamExt;
use geo::{Distance, Haversine};
use log::{debug, error, info};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout_at};
use tracing::{Instrument, field, info_span, warn};
use url::Url;

type E = OsmEntryId;

/// One durable raw event handed to a worker, with the wall-clock stamps the
/// partition forwarder captured: when it was queued (for channel-residency
/// timing) and its wire send time (for end-to-end timing).
struct Dispatch {
    queued_at: web_time::SystemTime,
    sent_at: Option<web_time::SystemTime>,
    payload: Payload,
    message: jetstream::Message,
}

/// "start-end" (inclusive), or a single partition.
fn parse_partitions(s: &str) -> core::result::Result<RangeInclusive<u64>, String> {
    let (start, end) = s.split_once('-').unwrap_or((s, s));

    let start: u64 = start
        .trim()
        .parse()
        .map_err(|e| format!("bad start: {e}"))?;
    let end: u64 = end.trim().parse().map_err(|e| format!("bad end: {e}"))?;

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
    /// URL of the NATS server
    #[arg(short, env, long)]
    nats: Url,

    /// Valkey primaries, comma-separated. Vehicles are spread across them by
    /// rendezvous hash, so the order carries no meaning and every binary that
    /// touches the history must be given the same set.
    #[arg(short, env, long, value_delimiter = ',')]
    redis: Vec<Url>,

    /// The vehicle partitions this pod owns, as an inclusive range
    /// ("0-255"). Assignment is static: give every pod a disjoint slice and
    /// cover 0-1023 between them. Alternatively, derive the slice from a
    /// StatefulSet identity via --pod-name and --fleet.
    #[arg(short, env, long, value_parser = parse_partitions, conflicts_with_all = ["pod_name", "fleet"])]
    partitions: Option<RangeInclusive<u64>>,

    /// This pod's StatefulSet name (e.g. `orchestrator-3`): the trailing
    /// ordinal picks its slice of the partition space. Pair with --fleet.
    #[arg(long, env = "POD_NAME", requires = "fleet")]
    pod_name: Option<String>,

    /// Total pods in the StatefulSet. Every pod must be given the same
    /// value, or their slices overlap or leave gaps.
    #[arg(long, env, requires = "pod_name")]
    fleet: Option<u64>,

    /// How many raw streams the partition space divides across, fleet-wide.
    /// Fixed config: revisions are stream sequences, so remapping partitions
    /// to different streams is a migration, not a tuning knob.
    #[arg(long, env, default_value_t = 4)]
    streams: u64,

    /// Unacknowledged events each partition's consumer may hold — the
    /// backlog knob. Under saturation the stream buffers and this throttles
    /// delivery; nothing is dropped.
    #[arg(long, env, default_value_t = 2048)]
    max_ack_pending: i64,

    /// How long the broker waits for an ack before redelivering an event.
    /// Generous: the pipeline retries transient failures inline, and a
    /// redelivered duplicate is dropped by the lane gate or deduplicated
    /// downstream by revision.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "60s")]
    ack_wait: Duration,

    /// How long the matched stream retains emissions. Nothing consumes it
    /// destructively; size it for the reconciler's worst lag.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "15m")]
    matched_retention: Duration,

    /// The subject prefix matchers serve requests on. Each request routes to
    /// `<prefix>.<geohash>` — the geographic shard of the event's position —
    /// so this pod's vehicles reach whichever matchers own the ground beneath
    /// them. The orchestrator itself stays geography-blind beyond this.
    #[arg(long = "match-prefix", env, default_value = "events.match")]
    match_prefix: String,

    /// How long to wait for a matcher's reply before re-driving the request.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "5s")]
    solve_timeout: Duration,

    /// Re-drives after the first attempt before the pipeline backs off and
    /// starts over. Replies are idempotent downstream (layers merge by
    /// timestamp and resolve by revision), so a duplicate solve from a late
    /// reply is convergence, not conflict.
    #[arg(long, env, default_value_t = 3)]
    solve_retries: usize,

    /// The number of history entries a vehicle's context draws from.
    #[arg(short, long = "context-window", env, default_value = "10")]
    context_window: usize,

    /// Points older than this will be discarded from history, regardless
    /// of if it's within the KV store, or not.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "120s")]
    gap: Duration,

    /// Consecutive points further away than this will be treated as a "teleport",
    /// and dropped along with everything older.
    #[arg(long, env, default_value = "2000")]
    jump_distance: f64,

    /// How many workers to fan vehicles across. Each vehicle is pinned to one
    /// by hash, so its events stay ordered on a worker that owns their trip
    /// and history lanes outright — the maps need no locks. A worker holds
    /// its lane for a whole solve round trip, so this is the pod's in-flight
    /// solve bound; workers are tokio tasks, priced accordingly.
    #[arg(short, env, long, default_value = "64")]
    workers: usize,

    /// Vehicles each worker keeps trip and history lanes for, before the
    /// least recently used is evicted.
    ///
    /// Vehicles are hash-spread across workers, so the fleet-wide bound is
    /// this times `workers`. Size it above the concurrent vehicles the owned
    /// partitions carry: evicting a live vehicle costs a Valkey re-warm and a
    /// trip restart. Rounded up to a power of two.
    #[arg(long, env, default_value_t = 1024)]
    vehicle_cache: usize,

    /// The number of events to keep in each vehicle's durable Valkey tail —
    /// the failover recovery source.
    #[arg(long, env, default_value_t = 25)]
    history: usize,

    /// Batch size for Valkey tail writes.
    #[arg(long, env, default_value_t = 128)]
    batch_size: usize,

    /// Batch timeout for Valkey tail writes.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "20ms")]
    batch_timeout: Duration,
}

/// The slice of the partition space this pod owns: explicit, or derived from
/// its StatefulSet identity — contiguous ordinal blocks, the last pod taking
/// the remainder.
fn owned_partitions(args: &Args) -> Result<RangeInclusive<u64>> {
    if let Some(partitions) = &args.partitions {
        return Ok(partitions.clone());
    }

    let (Some(name), Some(fleet)) = (&args.pod_name, args.fleet) else {
        anyhow::bail!("provide --partitions, or --pod-name with --fleet");
    };
    anyhow::ensure!(
        (1..=PARTITIONS).contains(&fleet),
        "fleet size {fleet} outside 1..={PARTITIONS}"
    );

    let ordinal: u64 = name
        .rsplit('-')
        .next()
        .unwrap_or_default()
        .parse()
        .with_context(|| format!("pod name {name:?} carries no trailing ordinal"))?;
    anyhow::ensure!(
        ordinal < fleet,
        "ordinal {ordinal} outside fleet of {fleet}"
    );

    let chunk = PARTITIONS / fleet;
    let start = ordinal * chunk;
    let end = if ordinal == fleet - 1 {
        PARTITIONS - 1
    } else {
        (ordinal + 1) * chunk - 1
    };
    Ok(start..=end)
}

/// How one event left the pipeline: fully processed, or deliberately
/// dropped. Either way it is acknowledged — transient failures never reach
/// this type, they retry inside the pipeline.
enum Processed {
    Done,
    Dropped(&'static str),
}

/// Everything one worker owns: its vehicles' trip and history lanes, plus
/// handles to the stores and the bus. Workers share nothing, so a vehicle's
/// events serialize on its worker with no locks anywhere.
struct Worker {
    kv: RedisStore<RawEvent>,
    client: async_nats::Client,
    stream: jetstream::Context,
    archive: mpsc::Sender<(RawEvent, oneshot::Sender<()>)>,
    match_prefix: String,

    gap: chrono::TimeDelta,
    jump_distance: f64,
    context_window: usize,
    solve_timeout: Duration,
    solve_retries: usize,

    trips: HashCache<VehicleId, Trip<E>>,
    histories: HashCache<VehicleId, Vec<RawEvent>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _telemetry = routers_realtime::telemetry::init("routers-orchestrator");

    let args = Args::parse();
    info!("orchestrator started: {:?}", args);

    let nats_url = ServerAddr::from_url(args.nats.clone()).context("could not create NATS url")?;

    let client = ConnectOptions::new()
        .name("OrchestratorService")
        .connect(nats_url)
        .await
        .context("could not connect to NATS")?;
    let stream = jetstream::new(client.clone());

    topology::ensure_output_stream(
        &stream,
        &topology::OutputConfig {
            max_age: args.matched_retention,
        },
    )
    .await?;

    let gap = chrono::Duration::from_std(args.gap).context("gap out of range")?;

    // Connected once, then cloned per worker: the clone shares the multiplexed
    // sockets, so the pod holds one connection per primary rather than one per
    // primary per worker.
    let store = RedisStore::<RawEvent>::new(&args.redis)
        .await
        .context("could not connect to redis store")?;

    // The sole durable writer of raw tails, batched like the historian this
    // pipeline absorbed. A failed flush retries until Valkey returns — the
    // tail is the failover recovery source, so acks wait on it.
    let (archive_tx, mut archive_rx) = mpsc::channel::<(RawEvent, oneshot::Sender<()>)>(8192);
    {
        let mut kv = store.clone();
        let (history, batch_size, batch_timeout) =
            (args.history, args.batch_size, args.batch_timeout);

        tokio::spawn(async move {
            let mut batch: Vec<RawEvent> = Vec::with_capacity(batch_size);
            let mut completions: Vec<oneshot::Sender<()>> = Vec::with_capacity(batch_size);

            while let Some((event, done)) = archive_rx.recv().await {
                batch.clear();
                completions.clear();
                batch.push(event);
                completions.push(done);

                let deadline = Instant::now() + batch_timeout;
                while batch.len() < batch_size {
                    match timeout_at(deadline, archive_rx.recv()).await {
                        Ok(Some((event, done))) => {
                            batch.push(event);
                            completions.push(done);
                        }
                        Ok(None) | Err(_) => break,
                    }
                }

                let mut attempt: u32 = 0;
                while let Err(err) = kv
                    .write_many(&batch, history)
                    .instrument(info_span!("archive", events = batch.len()))
                    .await
                {
                    attempt += 1;
                    error!("archive write failed (attempt {attempt}): {err}");
                    tokio::time::sleep(
                        (Duration::from_millis(250) * attempt).min(Duration::from_secs(5)),
                    )
                    .await;
                }

                for done in completions.drain(..) {
                    let _ = done.send(());
                }
            }
        });
    }

    let mut handles = Vec::with_capacity(args.workers);
    let mut txs = Vec::with_capacity(args.workers);

    for _ in 0..args.workers {
        let (tx, mut rx) = mpsc::channel::<Dispatch>(1024);
        txs.push(tx);

        let mut worker = Worker {
            kv: store.clone(),
            client: client.clone(),
            stream: stream.clone(),
            archive: archive_tx.clone(),
            match_prefix: args.match_prefix.clone(),
            gap,
            jump_distance: args.jump_distance,
            context_window: args.context_window,
            solve_timeout: args.solve_timeout,
            solve_retries: args.solve_retries,
            trips: HashCache::with_capacity(0, args.vehicle_cache),
            histories: HashCache::with_capacity(0, args.vehicle_cache),
        };

        handles.push(tokio::spawn(async move {
            while let Some(Dispatch {
                queued_at,
                sent_at,
                payload,
                message,
            }) = rx.recv().await
            {
                bus::span_between("worker_wait", queued_at, bus::wallclock());

                let vehicle_id = payload.vehicle_id;
                let revision = message
                    .info()
                    .map(|info| info.stream_sequence)
                    .unwrap_or_default();

                let span = info_span!(
                    "orchestrate",
                    continuation = field::Empty,
                    fresh = field::Empty,
                    cut = field::Empty,
                    attempts = field::Empty,
                );

                // Never drop, never reorder: a transient failure backs off
                // and starts the event over, holding this vehicle's lane
                // (and, via max_ack_pending, eventually the partition) —
                // saturation builds a backlog in the stream instead.
                let mut attempt: u32 = 0;
                let outcome = loop {
                    match worker
                        .process(&payload, revision, sent_at)
                        .instrument(span.clone())
                        .await
                    {
                        Ok(outcome) => break outcome,
                        Err(err) => {
                            attempt += 1;
                            warn!("{vehicle_id}: pipeline attempt {attempt} failed: {err:#}");
                            tokio::time::sleep(
                                (Duration::from_millis(250) * attempt).min(Duration::from_secs(5)),
                            )
                            .await;
                        }
                    }
                };

                if let Processed::Dropped(reason) = outcome {
                    debug!("{vehicle_id}: event dropped ({reason})");
                }

                if let Err(err) = message.ack().await {
                    error!("{vehicle_id}: could not ack event: {err}");
                }
            }
        }));
    }

    // One forwarder per owned partition: pull the durable consumer, decode,
    // and pin to the vehicle's worker. Poison messages (undecodable) are
    // acked away — redelivering them can never succeed.
    let owned = owned_partitions(&args)?;
    let raw_cfg = topology::RawConfig {
        streams: args.streams,
        max_ack_pending: args.max_ack_pending,
        ack_wait: args.ack_wait,
        ..Default::default()
    };
    let mut forwarders = Vec::new();
    for partition in owned.clone() {
        let index = topology::raw_stream_index(partition, args.streams);
        let raw = topology::ensure_raw_stream(&stream, index, args.streams, &raw_cfg).await?;
        let consumer = topology::raw_consumer(&raw, partition, &raw_cfg, None).await?;

        let txs = txs.clone();
        forwarders.push(tokio::spawn(async move {
            let mut messages = match consumer.messages().await {
                Ok(messages) => messages,
                Err(err) => {
                    error!("partition {partition}: could not pull: {err}");
                    return;
                }
            };

            while let Some(next) = messages.next().await {
                let message = match next {
                    Ok(message) => message,
                    Err(err) => {
                        error!("partition {partition}: pull error: {err}");
                        continue;
                    }
                };

                bus::inbound(message.subject.as_str(), message.headers.as_ref());
                let sent_at = bus::last_sent_at();

                let payload = match Payload::decode(&message.payload) {
                    Ok(payload) => payload,
                    Err(err) => {
                        warn!("partition {partition}: acking poison event: {err}");
                        let _ = message.ack().await;
                        continue;
                    }
                };

                // The stable path (not `DefaultHasher`): worker pinning is
                // the same per-vehicle spread the partition scheme derives,
                // so the two never disagree on a Rust release boundary.
                let worker = partition::mix(payload.vehicle_id.0) as usize % txs.len();

                let dispatch = Dispatch {
                    queued_at: bus::wallclock(),
                    sent_at,
                    payload,
                    message,
                };
                if txs[worker].send(dispatch).await.is_err() {
                    return;
                }
            }
        }));
    }

    info!(
        "consuming partitions {:?} across {} stream(s)",
        owned, args.streams
    );

    for forwarder in forwarders {
        forwarder.await.ok();
    }

    // Dropping the senders drains each worker before the process exits.
    drop(txs);
    for handle in handles {
        handle.await.ok();
    }

    Ok(())
}

impl Worker {
    /// Run one event through the pipeline: warm and gate the history lane,
    /// build the context, solve over req/res, durably publish the emission,
    /// commit the resume state, durably archive the raw tail — then the
    /// caller acks. Errors are transients: the caller retries the whole
    /// pipeline, and every step tolerates being re-run (the publish carries
    /// the same revision, the archive tolerates a duplicate append).
    async fn process(
        &mut self,
        payload: &Payload,
        revision: u64,
        sent_at: Option<web_time::SystemTime>,
    ) -> Result<Processed> {
        let vehicle_id = payload.vehicle_id;

        // Warm the lane once per vehicle per ownership — the only Valkey
        // read, off the per-event hot path.
        if self.histories.get(&vehicle_id).is_none() {
            let mut warmed = self
                .kv
                .get_many(&vehicle_id, self.context_window * 3)
                .instrument(info_span!("lane_warm"))
                .await
                .context("could not warm history lane")?;
            warmed.sort_by_key(|event| event.timestamp);

            match self.histories.entry(vehicle_id) {
                Entry::Occupied(mut entry) => {
                    entry.put(warmed);
                }
                Entry::Vacant(entry) => {
                    entry.put_entry(warmed);
                }
            }
        }

        // The lane gate: supplier timestamps are per-vehicle monotonic, so a
        // regression is stale data — and a redelivery of an event whose ack
        // was lost lands here too, making re-processing idempotent.
        let history = {
            let guard = self.histories.get(&vehicle_id).expect("lane warmed above");
            guard.get().clone()
        };
        if let Some(last) = history.last()
            && payload.timestamp <= last.timestamp
        {
            return Ok(Processed::Dropped("stale_or_duplicate"));
        }

        let context = self.create_context(history, payload);

        // Route to the ground beneath the event: the matcher owning the
        // head's shard solves it, degrading a foreign resume itself.
        let subject = format!(
            "{}.{}",
            self.match_prefix,
            routers_realtime::event::shard_of(payload.point)
        );

        // The vehicle's lane holds through the round trip: its next event
        // cannot overtake this one, so a stale solve can never overwrite a
        // fresh trip. Other vehicles overlap on other workers.
        let Some(reply) = solve(
            &self.client,
            &subject,
            &context,
            self.solve_timeout,
            self.solve_retries,
        )
        .await
        else {
            anyhow::bail!("no matcher reply after retries");
        };

        if let MatchReply::Solved { mut diff, trip } = reply {
            // The revision is the ingest stream sequence: broker-assigned,
            // monotonic per vehicle, and identical across re-drives — the
            // total order competing solves resolve by.
            diff.revision = revision;

            let matched = MatchedEvent { vehicle_id, diff };
            let bytes = matched.encode().context("could not encode emission")?;

            let subject = topology::output_subject(partition::partition_of(vehicle_id));
            self.stream
                .publish_with_headers(subject, bus::outbound(), bytes.into())
                .instrument(info_span!("publish_matched"))
                .await
                .context("could not publish emission")?
                .await
                .context("emission unacknowledged")?;

            if let Some(sent_at) = sent_at {
                bus::span_between("event_to_match", sent_at, bus::wallclock());
            }

            // Commit after the durable publish: a crash in between re-drives
            // the whole event, never strands a trip ahead of its emissions.
            match self.trips.entry(vehicle_id) {
                Entry::Occupied(mut entry) => {
                    entry.put(trip);
                }
                Entry::Vacant(entry) => {
                    entry.put_entry(trip);
                }
            }
        }

        // The raw tail is the failover recovery source: durably written
        // before the ack, batched with everyone else's events.
        let event = RawEvent {
            vehicle_id,
            point: payload.point,
            timestamp: payload.timestamp,
        };

        let (done, flushed) = oneshot::channel();
        self.archive
            .send((event.clone(), done))
            .await
            .map_err(|_| anyhow!("archive writer gone"))?;
        flushed
            .instrument(info_span!("archive_wait"))
            .await
            .map_err(|_| anyhow!("archive writer dropped the batch"))?;

        // Only now does the event enter the lane: everything behind the gate
        // is durably recorded, so a redelivery can trust the drop.
        if let Some(mut guard) = self.histories.get(&vehicle_id) {
            let lane = guard.get_mut();
            lane.push(event);

            let bound = self.context_window * 3;
            if lane.len() > bound {
                let excess = lane.len() - bound;
                lane.drain(..excess);
            }
        }

        Ok(Processed::Done)
    }

    /// Assemble the vehicle's match context from its (oldest-first) history
    /// lane and the live event: gap/teleport cut, then reconcile against the
    /// committed trip.
    fn create_context(&self, mut entries: Vec<RawEvent>, payload: &Payload) -> MatchContext<E> {
        let Payload {
            vehicle_id,
            timestamp,
            point,
        } = *payload;

        entries.retain(|event| event.timestamp <= timestamp);
        entries.sort_by_key(|event| std::cmp::Reverse(event.timestamp));
        entries.truncate(self.context_window);

        let fetched = entries.len();
        let context = entries
            .into_iter()
            .inspect(|v| debug!("event: {:?}", v))
            .scan((point, timestamp), |(prev_p, prev_ts), event: RawEvent| {
                let duration = (*prev_ts - event.timestamp).abs();
                let distance = Haversine.distance(*prev_p, event.point);

                if duration <= self.gap && distance <= self.jump_distance {
                    *prev_p = event.point;
                    *prev_ts = event.timestamp;
                    Some(event)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let cut = fetched - context.len();
        if cut > 0 {
            info_span!("history_cut", reason = "gap_or_teleport").in_scope(|| {});
        }

        let mut history: Vec<RawEvent> = std::iter::once(RawEvent {
            vehicle_id,
            point,
            timestamp,
        })
        .chain(context)
        .collect();

        history.sort_by_key(|event| event.timestamp);
        history.dedup_by_key(|event| event.timestamp);

        let origins = history
            .into_iter()
            .map(|event| Origin::new(event.point, event.timestamp.timestamp_micros()))
            .collect::<Vec<_>>();

        let previous = self.trips.get(&vehicle_id).map(|trip| trip.get().clone());
        let continuation =
            info_span!("reconcile").in_scope(|| Continuation::reconcile(previous, &origins));

        let span = tracing::Span::current();
        span.record("cut", cut);
        match &continuation {
            Continuation::Resume { fresh, .. } => {
                span.record("continuation", "resume");
                span.record("fresh", fresh.len());
            }
            Continuation::Restart { fresh } => {
                span.record("continuation", "restart");
                span.record("fresh", fresh.len());
            }
        }

        MatchContext {
            vehicle_id,
            continuation,
        }
    }
}

/// Ask a matcher for one context's solve, re-driving on timeout or transport
/// error. `None` when every attempt failed; the caller treats that as a
/// transient and starts the event over — nothing is dropped.
async fn solve(
    client: &async_nats::Client,
    subject: &str,
    context: &MatchContext<E>,
    timeout: Duration,
    retries: usize,
) -> Option<MatchReply<E>> {
    let payload = match context.encode() {
        Ok(payload) => payload,
        Err(err) => {
            error!("could not encode match context: {err:#}");
            return None;
        }
    };

    for attempt in 0..=retries {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(250) * attempt as u32).await;
        }

        let request = client.request_with_headers(
            subject.to_string(),
            bus::outbound(),
            payload.clone().into(),
        );

        match tokio::time::timeout(timeout, request).await {
            Ok(Ok(message)) => {
                tracing::Span::current().record("attempts", attempt as u64 + 1);

                match MatchReply::<E>::decode(&message.payload) {
                    Ok(reply) => return Some(reply),
                    // A decode failure is a version skew, not a transient:
                    // re-driving it would only re-fail.
                    Err(err) => {
                        error!("undecodable reply: {err:#}");
                        return None;
                    }
                }
            }
            Ok(Err(err)) => warn!("solve request failed (attempt {attempt}): {err}"),
            Err(_) => warn!("solve request timed out (attempt {attempt})"),
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        let base = [
            "orchestrator",
            "--nats",
            "nats://localhost",
            "--redis",
            "redis://localhost",
        ];
        Args::parse_from(base.iter().copied().chain(extra.iter().copied()))
    }

    /// Every fleet size slices the space disjointly and completely, ordinal
    /// blocks in order, the last pod absorbing the remainder.
    #[test]
    fn fleet_slices_cover_the_space_disjointly() {
        for fleet in [1u64, 3, 4, 7, 16] {
            let mut next = 0;
            for ordinal in 0..fleet {
                let name = format!("orchestrator-{ordinal}");
                let range =
                    owned_partitions(&args(&["--pod-name", &name, "--fleet", &fleet.to_string()]))
                        .unwrap();

                assert_eq!(*range.start(), next, "fleet {fleet} ordinal {ordinal}");
                next = range.end() + 1;
            }
            assert_eq!(next, PARTITIONS, "fleet {fleet} must cover the space");
        }
    }

    #[test]
    fn partition_ranges_parse_and_validate() {
        assert_eq!(parse_partitions("0-255").unwrap(), 0..=255);
        assert_eq!(parse_partitions("7").unwrap(), 7..=7);
        assert!(parse_partitions("5-4").is_err());
        assert!(parse_partitions("0-1024").is_err());
    }

    #[test]
    fn fleet_rejects_a_nameless_or_oversized_ordinal() {
        assert!(owned_partitions(&args(&["--pod-name", "orchestrator", "--fleet", "4"])).is_err());
        assert!(
            owned_partitions(&args(&["--pod-name", "orchestrator-4", "--fleet", "4"])).is_err()
        );
        assert!(owned_partitions(&args(&[])).is_err());
    }
}

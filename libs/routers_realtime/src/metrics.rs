//! Bounded-label metrics.
//!
//! One [`Metrics`] handle carries every OpenTelemetry instrument the realtime
//! services publish. It is [`Clone`] (each instrument is a cheap `Arc`), so a
//! binary builds it once and hands clones to its workers. Every attribute is
//! drawn from a bounded alphabet — region, lane, outcome/reason, partition
//! class — never an unbounded identity, so cardinality stays capped. Until
//! [`telemetry`](crate::telemetry) installs a provider the meter is a no-op.

use alloc::sync::Arc;

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, ObservableGauge};
use opentelemetry::{KeyValue, global};

/// The meter name every realtime instrument is registered under.
const METER: &str = "routers_realtime";

/// How many bounded classes [`Metrics::partition_class`] folds the partitions into.
const PARTITION_CLASSES: u16 = 16;

/// Latency buckets in seconds, 1 ms to 2 min; the SDK default is scaled for milliseconds.
const SECONDS_BUCKETS: [f64; 16] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// Payload buckets in bytes, 1 KiB to 16 MiB.
const BYTES_BUCKETS: [f64; 11] = [
    1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 2097152.0, 4194304.0, 8388608.0,
    12582912.0, 16777216.0,
];

/// A single reading of one admission credit scope. `region` is `None` for the
/// process-wide scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionRow {
    /// The region this reading is for, or `None` for the global aggregate.
    pub region: Option<String>,
    /// Outstanding (admitted, not-yet-released) jobs at the reading.
    pub jobs: u64,
    /// Demand held waiting for admission at the reading.
    pub waiting: u64,
}

/// Handles for the admission gauges; keep them alive or the gauges stop reporting.
#[must_use = "the admission gauges stop reporting when these handles are dropped"]
pub struct AdmissionGauges {
    _jobs_outstanding: ObservableGauge<u64>,
    _admission_waiting: ObservableGauge<u64>,
}

/// Every bounded-label instrument the realtime services publish.
#[derive(Clone)]
pub struct Metrics {
    meter: Meter,

    offered_observations: Counter<u64>,
    queued_observations: Counter<u64>,
    suppressed_observations: Counter<u64>,
    deferred_observations: Counter<u64>,
    poison_observations: Counter<u64>,

    dispatch_held: Counter<u64>,
    jobs_claimed: Counter<u64>,
    job_bytes: Histogram<u64>,
    result_bytes: Histogram<u64>,
    output_bytes: Histogram<u64>,

    results_received: Counter<u64>,
    parked_results: Counter<u64>,
    quarantined_results: Counter<u64>,
    rejected_results: Counter<u64>,

    checkpoint_commit_seconds: Histogram<f64>,
    completions: Counter<u64>,

    frontier_lag: Gauge<u64>,
    oldest_pending_age_seconds: Gauge<f64>,

    solve_seconds: Histogram<f64>,
    queue_wait_seconds: Histogram<f64>,
    result_publish_seconds: Histogram<f64>,
    job_round_trip_seconds: Histogram<f64>,
    graph_ready: Gauge<u64>,
    vehicles_tracked: Gauge<u64>,
    pending_observations: Gauge<u64>,
    active_jobs: Gauge<u64>,

    materialized_outputs: Counter<u64>,
}

impl core::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Metrics").finish_non_exhaustive()
    }
}

impl Metrics {
    /// Build the instruments from the process's global meter.
    #[must_use]
    pub fn new() -> Self {
        Self::from_meter(global::meter(METER))
    }

    /// A handle whose instruments record nothing measurable until a provider is installed.
    #[must_use]
    pub fn noop() -> Self {
        Self::from_meter(global::meter(METER))
    }

    /// Build the instruments from an explicit meter.
    #[must_use]
    pub fn from_meter(meter: Meter) -> Self {
        let offered_observations = meter
            .u64_counter("offered_observations")
            .with_description("Raw observations offered to the orchestrator intake.")
            .build();
        let queued_observations = meter
            .u64_counter("queued_observations")
            .with_description("Observations appended to a vehicle FIFO for solving.")
            .build();
        let suppressed_observations = meter
            .u64_counter("suppressed_observations")
            .with_description("Valid observations acknowledged because durable state covers them.")
            .build();
        let deferred_observations = meter
            .u64_counter("deferred_observations")
            .with_description("Valid observations left broker-owned for later redelivery.")
            .build();
        let poison_observations = meter
            .u64_counter("poison_observations")
            .with_description("Unusable raw messages acknowledged and dropped.")
            .build();
        let dispatch_held = meter
            .u64_counter("dispatch_held")
            .with_description("Dispatch attempts held back by admission or a transient fault.")
            .build();
        let jobs_claimed = meter
            .u64_counter("jobs_claimed")
            .with_description("Solve jobs published to the solve plane.")
            .build();
        let job_bytes = meter
            .u64_histogram("job_bytes")
            .with_description("Encoded size of a dispatched solve job.")
            .with_unit("By")
            .with_boundaries(BYTES_BUCKETS.to_vec())
            .build();
        let result_bytes = meter
            .u64_histogram("result_bytes")
            .with_description("Encoded size of a published solve result.")
            .with_unit("By")
            .with_boundaries(BYTES_BUCKETS.to_vec())
            .build();
        let output_bytes = meter
            .u64_histogram("output_bytes")
            .with_description("Encoded size of the committed outputs of one decision.")
            .with_unit("By")
            .with_boundaries(BYTES_BUCKETS.to_vec())
            .build();
        let results_received = meter
            .u64_counter("results_received")
            .with_description("Solve results accepted as the answer to an active job, by outcome.")
            .build();
        let parked_results = meter
            .u64_counter("parked_results")
            .with_description("Early solve results parked for replay after dispatch.")
            .build();
        let quarantined_results = meter
            .u64_counter("quarantined_results")
            .with_description("Same-identity solve results kept out of the commit path.")
            .build();
        let rejected_results = meter
            .u64_counter("rejected_results")
            .with_description("Obsolete solve results acknowledged and dropped.")
            .build();
        let checkpoint_commit_seconds = meter
            .f64_histogram("checkpoint_commit_seconds")
            .with_description("Wall time to prepare, publish, and promote one commit.")
            .with_unit("s")
            .with_boundaries(SECONDS_BUCKETS.to_vec())
            .build();
        let completions = meter
            .u64_counter("completions")
            .with_description("Committed decisions by outcome (matched|terminal|reset) and reason.")
            .build();
        let frontier_lag = meter
            .u64_gauge("frontier_lag")
            .with_description("Distance between the highest seen and completed raw sequence.")
            .build();
        let oldest_pending_age_seconds = meter
            .f64_gauge("oldest_pending_age_seconds")
            .with_description("Age of the oldest observation still awaiting a durable answer.")
            .with_unit("s")
            .build();
        let solve_seconds = meter
            .f64_histogram("solve_seconds")
            .with_description("CPU time a matcher spent solving one job, by outcome.")
            .with_unit("s")
            .with_boundaries(SECONDS_BUCKETS.to_vec())
            .build();
        let queue_wait_seconds = meter
            .f64_histogram("queue_wait_seconds")
            .with_description("Time a job waited on the solve plane before a matcher claimed it.")
            .with_unit("s")
            .with_boundaries(SECONDS_BUCKETS.to_vec())
            .build();
        let result_publish_seconds = meter
            .f64_histogram("result_publish_seconds")
            .with_description("Time to publish a solve result and have the broker acknowledge it.")
            .with_unit("s")
            .with_boundaries(SECONDS_BUCKETS.to_vec())
            .build();
        let graph_ready = meter
            .u64_gauge("graph_ready")
            .with_description("1 while a matcher replica serves a usable graph, else 0.")
            .build();
        let job_round_trip_seconds = meter
            .f64_histogram("job_round_trip_seconds")
            .with_description("Dispatch to accepted result for one job, by region and outcome.")
            .with_unit("s")
            .with_boundaries(SECONDS_BUCKETS.to_vec())
            .build();
        let vehicles_tracked = meter
            .u64_gauge("vehicles_tracked")
            .with_description("Vehicles the scheduler currently holds state for.")
            .build();
        let pending_observations = meter
            .u64_gauge("pending_observations")
            .with_description("Observations queued across all vehicle FIFOs.")
            .build();
        let active_jobs = meter
            .u64_gauge("active_jobs")
            .with_description("Vehicles with a solve job in flight.")
            .build();
        let materialized_outputs = meter
            .u64_counter("materialized_outputs")
            .with_description("Committed outputs applied to the served view, by applied kind.")
            .build();

        Self {
            meter,
            offered_observations,
            queued_observations,
            suppressed_observations,
            deferred_observations,
            poison_observations,
            dispatch_held,
            jobs_claimed,
            job_bytes,
            result_bytes,
            output_bytes,
            results_received,
            parked_results,
            quarantined_results,
            rejected_results,
            checkpoint_commit_seconds,
            completions,
            frontier_lag,
            oldest_pending_age_seconds,
            solve_seconds,
            queue_wait_seconds,
            result_publish_seconds,
            job_round_trip_seconds,
            graph_ready,
            vehicles_tracked,
            pending_observations,
            active_jobs,
            materialized_outputs,
        }
    }

    /// Bucket a partition into one of the bounded partition classes.
    #[must_use]
    pub fn partition_class(partition: u16) -> String {
        format!("c{}", partition % PARTITION_CLASSES)
    }

    /// One raw observation reached the intake, tagged by its partition class.
    pub fn observed(&self, partition_class: &str) {
        self.offered_observations
            .add(1, &[class_attr(partition_class)]);
    }

    /// One observation was queued for solving.
    pub fn queued(&self) {
        self.queued_observations.add(1, &[]);
    }

    /// One valid observation was suppressed before queueing, by reason.
    pub fn suppressed(&self, reason: &str) {
        self.suppressed_observations.add(1, &[reason_attr(reason)]);
    }

    /// One valid observation remained broker-owned for later redelivery, by reason.
    pub fn deferred(&self, reason: &str) {
        self.deferred_observations.add(1, &[reason_attr(reason)]);
    }

    /// One unusable raw message was dropped, by reason.
    pub fn poison(&self, reason: &str) {
        self.poison_observations.add(1, &[reason_attr(reason)]);
    }

    /// One dispatch was held back by admission (or a transient fault), by region.
    pub fn held(&self, region: &str) {
        self.dispatch_held.add(1, &[region_attr(region)]);
    }

    /// One solve job was published, by region and lane.
    pub fn dispatched(&self, region: &str, lane: u8) {
        self.jobs_claimed
            .add(1, &[region_attr(region), lane_attr(lane)]);
    }

    /// The encoded size of a dispatched job, by region.
    pub fn job_bytes(&self, region: &str, bytes: u64) {
        self.job_bytes.record(bytes, &[region_attr(region)]);
    }

    /// Encoded size of one published solve result, by region.
    pub fn result_bytes(&self, region: &str, bytes: u64) {
        self.result_bytes.record(bytes, &[region_attr(region)]);
    }

    /// Encoded size of one decision's committed outputs.
    pub fn output_bytes(&self, bytes: u64) {
        self.output_bytes.record(bytes, &[]);
    }

    /// One solve result accepted as an active job's answer, by region and outcome.
    pub fn result(&self, region: &str, outcome_kind: &str) {
        self.results_received
            .add(1, &[region_attr(region), outcome_attr(outcome_kind)]);
    }

    /// One early result was parked for replay.
    pub fn parked(&self) {
        self.parked_results.add(1, &[]);
    }

    /// One same-identity result was quarantined, by reason.
    pub fn quarantined(&self, reason: &str) {
        self.quarantined_results.add(1, &[reason_attr(reason)]);
    }

    /// One obsolete result was rejected, by reason.
    pub fn rejected(&self, reason: &str) {
        self.rejected_results.add(1, &[reason_attr(reason)]);
    }

    /// The wall time one commit took, by kind (`matched`|`terminal`|`reset`).
    pub fn commit_seconds(&self, kind: &str, secs: f64) {
        self.checkpoint_commit_seconds
            .record(secs, &[kind_attr(kind)]);
    }

    /// One committed decision, by outcome (`matched`|`terminal`|`reset`) and reason.
    pub fn completion(&self, outcome: &str, reason: &str) {
        self.completions
            .add(1, &[outcome_attr(outcome), reason_attr(reason)]);
    }

    /// The partition's current completion-frontier lag, by partition class.
    pub fn frontier_lag(&self, partition_class: &str, n: u64) {
        self.frontier_lag.record(n, &[class_attr(partition_class)]);
    }

    /// The age of the oldest still-pending observation, in seconds.
    pub fn oldest_pending_seconds(&self, secs: f64) {
        self.oldest_pending_age_seconds.record(secs, &[]);
    }

    /// The CPU time one solve took, by region and outcome kind.
    pub fn solve_seconds(&self, region: &str, outcome: &str, secs: f64) {
        self.solve_seconds
            .record(secs, &[region_attr(region), outcome_attr(outcome)]);
    }

    /// How long a job waited on the solve plane before it was claimed, by region and delivery attempt.
    pub fn queue_wait_seconds(&self, region: &str, redelivered: bool, secs: f64) {
        let delivery = if redelivered { "redelivered" } else { "first" };
        self.queue_wait_seconds.record(
            secs,
            &[region_attr(region), KeyValue::new("delivery", delivery)],
        );
    }

    /// How long publishing one result took, end to broker acknowledgement.
    pub fn result_publish_seconds(&self, secs: f64) {
        self.result_publish_seconds.record(secs, &[]);
    }

    /// Dispatch to accepted result for one job, by region and outcome.
    pub fn round_trip_seconds(&self, region: &str, outcome: &str, secs: f64) {
        self.job_round_trip_seconds
            .record(secs, &[region_attr(region), outcome_attr(outcome)]);
    }

    /// One partition's scheduler depth: tracked vehicles, queued observations, jobs in flight.
    pub fn depth(&self, partition_class: &str, vehicles: u64, pending: u64, active: u64) {
        let class = [class_attr(partition_class)];
        self.vehicles_tracked.record(vehicles, &class);
        self.pending_observations.record(pending, &class);
        self.active_jobs.record(active, &class);
    }

    /// A matcher replica's graph readiness, by region: `1` ready, `0` not.
    pub fn graph_ready(&self, region: &str, ready: u64) {
        self.graph_ready.record(ready, &[region_attr(region)]);
    }

    /// One committed output was applied to the served view, by applied kind.
    pub fn materialized(&self, applied_kind: &str) {
        self.materialized_outputs.add(1, &[kind_attr(applied_kind)]);
    }

    /// Register the two admission observable gauges (`jobs_outstanding`,
    /// `admission_waiting`), pulled from `snapshot` on every collection. The
    /// returned [`AdmissionGauges`] must be kept alive or the callbacks unregister.
    pub fn register_admission<F>(&self, snapshot: F) -> AdmissionGauges
    where
        F: Fn() -> Vec<AdmissionRow> + Send + Sync + 'static,
    {
        let snapshot = Arc::new(snapshot);

        let jobs_snapshot = Arc::clone(&snapshot);
        let jobs_outstanding = self
            .meter
            .u64_observable_gauge("jobs_outstanding")
            .with_description("Admitted, not-yet-released solve jobs, by region.")
            .with_callback(move |observer| {
                for row in jobs_snapshot().iter() {
                    observer.observe(row.jobs, &[scope_attr(row.region.as_deref())]);
                }
            })
            .build();

        let waiting_snapshot = Arc::clone(&snapshot);
        let admission_waiting = self
            .meter
            .u64_observable_gauge("admission_waiting")
            .with_description("Demand held waiting for admission, by region.")
            .with_callback(move |observer| {
                for row in waiting_snapshot().iter() {
                    observer.observe(row.waiting, &[scope_attr(row.region.as_deref())]);
                }
            })
            .build();

        AdmissionGauges {
            _jobs_outstanding: jobs_outstanding,
            _admission_waiting: admission_waiting,
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::noop()
    }
}

/// The label key for an admission scope: a region name, or `all` for the
/// process-wide scope. A closed alphabet, so the cardinality stays bounded.
fn scope_attr(region: Option<&str>) -> KeyValue {
    region_attr(region.unwrap_or("all"))
}

fn region_attr(region: &str) -> KeyValue {
    KeyValue::new("region", region.to_owned())
}

fn lane_attr(lane: u8) -> KeyValue {
    KeyValue::new("lane", i64::from(lane))
}

fn reason_attr(reason: &str) -> KeyValue {
    KeyValue::new("reason", reason.to_owned())
}

fn outcome_attr(outcome: &str) -> KeyValue {
    KeyValue::new("outcome", outcome.to_owned())
}

fn kind_attr(kind: &str) -> KeyValue {
    KeyValue::new("kind", kind.to_owned())
}

fn class_attr(class: &str) -> KeyValue {
    KeyValue::new("partition_class", class.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::collections::BTreeSet;
    use std::collections::HashMap;

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::metrics::data::{
        Gauge as GaugeData, Histogram as HistogramData, ResourceMetrics, Sum as SumData,
    };
    use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
    use opentelemetry_sdk::metrics::reader::MetricReader as _;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};

    /// A push exporter that does nothing; tests collect directly rather than export.
    #[derive(Debug, Default)]
    struct NoopExporter;

    impl PushMetricExporter for NoopExporter {
        async fn export(&self, _metrics: &mut ResourceMetrics) -> OTelSdkResult {
            Ok(())
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown(&self) -> OTelSdkResult {
            Ok(())
        }

        fn temporality(&self) -> Temporality {
            Temporality::Cumulative
        }
    }

    /// A test rig: an SDK meter provider plus the [`Metrics`] built from its meter.
    struct Rig {
        reader: PeriodicReader<NoopExporter>,
        metrics: Metrics,
    }

    impl Rig {
        fn new() -> Self {
            let reader = PeriodicReader::builder(NoopExporter).build();
            let provider = SdkMeterProvider::builder()
                .with_reader(reader.clone())
                .with_resource(Resource::builder().with_service_name("test").build())
                .build();
            let metrics = Metrics::from_meter(provider.meter(METER));
            Self { reader, metrics }
        }

        /// Collect once, returning each metric name mapped to its attribute pairs.
        fn collect(&self) -> HashMap<String, BTreeSet<(String, String)>> {
            let mut rm = ResourceMetrics {
                resource: Resource::builder().build(),
                scope_metrics: Vec::new(),
            };
            self.reader.collect(&mut rm).expect("collect");

            let mut out: HashMap<String, BTreeSet<(String, String)>> = HashMap::new();
            for scope in &rm.scope_metrics {
                for metric in &scope.metrics {
                    let entry = out.entry(metric.name.to_string()).or_default();
                    for pair in attribute_pairs(metric.data.as_ref()) {
                        entry.insert(pair);
                    }
                }
            }
            out
        }
    }

    /// Pull every data point's attributes out of one aggregation.
    fn attribute_pairs(
        data: &dyn opentelemetry_sdk::metrics::data::Aggregation,
    ) -> Vec<(String, String)> {
        let any = data.as_any();
        let mut pairs = Vec::new();
        if let Some(sum) = any.downcast_ref::<SumData<u64>>() {
            for dp in &sum.data_points {
                push_pairs(&mut pairs, &dp.attributes);
            }
        } else if let Some(gauge) = any.downcast_ref::<GaugeData<u64>>() {
            for dp in &gauge.data_points {
                push_pairs(&mut pairs, &dp.attributes);
            }
        } else if let Some(gauge) = any.downcast_ref::<GaugeData<f64>>() {
            for dp in &gauge.data_points {
                push_pairs(&mut pairs, &dp.attributes);
            }
        } else if let Some(hist) = any.downcast_ref::<HistogramData<u64>>() {
            for dp in &hist.data_points {
                push_pairs(&mut pairs, &dp.attributes);
            }
        } else if let Some(hist) = any.downcast_ref::<HistogramData<f64>>() {
            for dp in &hist.data_points {
                push_pairs(&mut pairs, &dp.attributes);
            }
        }
        pairs
    }

    fn push_pairs(pairs: &mut Vec<(String, String)>, attrs: &[KeyValue]) {
        for kv in attrs {
            pairs.push((kv.key.to_string(), kv.value.to_string()));
        }
    }

    #[test]
    fn records_every_instrument_name() {
        let rig = Rig::new();
        let m = &rig.metrics;

        m.observed("c0");
        m.queued();
        m.suppressed("committed");
        m.deferred("duplicate_owned");
        m.poison("bad-schema");
        m.held("syd");
        m.dispatched("syd", 0);
        m.job_bytes("syd", 1_234);
        m.result_bytes("syd", 80_000);
        m.output_bytes(512);
        m.result("syd", "solved");
        m.parked();
        m.quarantined("duplicate");
        m.rejected("stale");
        m.commit_seconds("matched", 0.01);
        m.completion("matched", "solved");
        m.frontier_lag("c0", 3);
        m.oldest_pending_seconds(2.5);
        m.solve_seconds("syd", "solved", 0.02);
        m.queue_wait_seconds("syd", false, 0.005);
        m.round_trip_seconds("syd", "solved", 0.2);
        m.depth("c0", 1, 2, 3);
        m.result_publish_seconds(0.003);
        m.graph_ready("syd", 1);
        m.materialized("inserted");

        let collected = rig.collect();
        for name in [
            "offered_observations",
            "queued_observations",
            "suppressed_observations",
            "deferred_observations",
            "poison_observations",
            "dispatch_held",
            "jobs_claimed",
            "job_bytes",
            "result_bytes",
            "output_bytes",
            "results_received",
            "parked_results",
            "quarantined_results",
            "rejected_results",
            "checkpoint_commit_seconds",
            "completions",
            "frontier_lag",
            "oldest_pending_age_seconds",
            "solve_seconds",
            "queue_wait_seconds",
            "result_publish_seconds",
            "job_round_trip_seconds",
            "graph_ready",
            "vehicles_tracked",
            "pending_observations",
            "active_jobs",
            "materialized_outputs",
        ] {
            assert!(collected.contains_key(name), "missing instrument {name}");
        }
    }

    #[test]
    fn labels_are_the_expected_bounded_keys() {
        let rig = Rig::new();
        let m = &rig.metrics;

        m.dispatched("syd", 2);
        m.completion("reset", "gap");
        m.solve_seconds("mel", "unanchored", 0.01);

        let collected = rig.collect();

        let claimed = &collected["jobs_claimed"];
        assert!(claimed.contains(&("region".to_owned(), "syd".to_owned())));
        assert!(claimed.contains(&("lane".to_owned(), "2".to_owned())));

        let completions = &collected["completions"];
        assert!(completions.contains(&("outcome".to_owned(), "reset".to_owned())));
        assert!(completions.contains(&("reason".to_owned(), "gap".to_owned())));

        let solve = &collected["solve_seconds"];
        assert!(solve.contains(&("region".to_owned(), "mel".to_owned())));
        assert!(solve.contains(&("outcome".to_owned(), "unanchored".to_owned())));
    }

    #[test]
    fn no_instrument_ever_carries_an_unbounded_identity_label() {
        let rig = Rig::new();
        let m = &rig.metrics;

        m.observed("c9");
        m.queued();
        m.suppressed("x");
        m.poison("x");
        m.held("r");
        m.dispatched("r", 255);
        m.job_bytes("r", 1);
        m.result("r", "solved");
        m.parked();
        m.quarantined("x");
        m.rejected("x");
        m.commit_seconds("terminal", 0.1);
        m.completion("terminal", "deadline_expired");
        m.frontier_lag("c1", 1);
        m.oldest_pending_seconds(1.0);
        m.solve_seconds("r", "solved", 0.1);
        m.queue_wait_seconds("r", true, 0.1);
        m.result_publish_seconds(0.1);
        m.graph_ready("r", 0);
        m.materialized("terminal");

        let collected = rig.collect();
        let forbidden = ["vehicle_id", "job_id", "vehicle", "job", "partition"];
        for (name, pairs) in &collected {
            for (key, _) in pairs {
                assert!(
                    !forbidden.contains(&key.as_str()),
                    "instrument {name} carries a forbidden label key {key}",
                );
            }
        }
    }

    #[test]
    fn admission_observable_gauges_pull_a_snapshot() {
        let rig = Rig::new();
        let _gauges = rig.metrics.register_admission(|| {
            vec![
                AdmissionRow {
                    region: Some("syd".to_owned()),
                    jobs: 4,
                    waiting: 1,
                },
                AdmissionRow {
                    region: None,
                    jobs: 4,
                    waiting: 1,
                },
            ]
        });

        let collected = rig.collect();
        assert!(collected.contains_key("jobs_outstanding"));
        assert!(collected.contains_key("admission_waiting"));

        let outstanding = &collected["jobs_outstanding"];
        assert!(outstanding.contains(&("region".to_owned(), "syd".to_owned())));
        assert!(outstanding.contains(&("region".to_owned(), "all".to_owned())));
    }

    #[test]
    fn partition_class_buckets_are_bounded() {
        let mut classes = BTreeSet::new();
        for partition in 0..crate::partition::PARTITIONS {
            classes.insert(Metrics::partition_class(partition as u16));
        }
        assert_eq!(classes.len(), PARTITION_CLASSES as usize);
    }

    #[test]
    fn noop_metrics_record_without_a_provider() {
        let m = Metrics::noop();
        m.observed("c0");
        m.completion("matched", "solved");
    }
}

# routers_realtime

Real-time map matching over NATS JetStream: a partitioned, crash-safe pipeline
that turns a stream of raw vehicle observations into a durable, revisable history
of matched output. This crate is the reference implementation of the design in
`routers-components-and-diagrams.md`; this README maps every spec component to
the code, and every module back to the decision behind it.

## The shape of the system

Four application roles pass messages over four JetStream planes. None of them
call each other directly — the bus is the only coupling.

```
ingress ──raw──▶ orchestrator ──jobs──▶ matcher ──results──▶ orchestrator ──output──▶ materializer
                 (owns continuity,                (stateless solve)        (commit + frontier)   (served view)
                  admission, commit)
```

* **Ingress** (`ingress.rs`, `bin/replay.rs`) validates observations, stamps
  receipt time, and publishes them to the stable raw partition.
* **Orchestrator** (`orchestrator/`, `bin/orchestrator.rs`) owns each vehicle's
  continuity, admits one job per vehicle, accepts only the expected result, and
  commits output plus checkpoint atomically.
* **Matcher** (`matcher/`, `bin/matcher.rs`) loads a pinned regional graph, pulls
  jobs within its CPU/payload budget, solves, and publishes a typed result
  before it acknowledges the job. It owns no vehicle state.
* **Materializer** (`materializer/`, `bin/materializer.rs`) is the one consumer
  that *is* the served history: it applies every committed output to a sink
  idempotently and recovers its own consumption independently.

## The four message planes

Subject, stream, and consumer names are wire law: every process must derive them
identically. They live in `topology/` (one module per plane) and are the single
source of truth; the table below is pulled from there.

| Plane | Subject | Stream | Retention | Consumer |
|---|---|---|---|---|
| Raw journal | `events.raw.p.<p>` | `EVENTS-RAW-<i>` (`streams` fixed, default 4; contiguous partition blocks) | `Limits`, `max_age` = raw retention (default 15m), `DiscardPolicy::Old` | durable pull `orchestrator-p<p>`, one subject, explicit ack, `ByStartSequence` from frontier + 1 on recovery |
| Solve jobs | `solve.v1.g.<graph>.r.<region>.q.<lane>` | `SOLVE-JOBS-<region>` (subjects `solve.v1.g.*.r.<region>.q.*`) | `WorkQueue`, `max_age` = job TTL (default 60s) | durable pull `matchers-g<graph>-r<region>`, filter `solve.v1.g.<graph>.r.<region>.q.>`, `max_deliver` 3, `ack_wait` 30s, shared by all replicas |
| Solve results | `solve-result.v1.p.<p>` | `SOLVE-RESULTS` (subjects `solve-result.v1.p.>`) | `Limits`, `max_age` default 10m | durable pull `orchestrator-results-p<p>`, one subject, `ack_wait` 30s |
| Committed output | `events.matched.v1.p.<p>` | `EVENTS-MATCHED` (subjects `events.matched.v1.p.>`) | `Limits`, `max_age` default 15m | materializer durable pull `materializer` (filter `events.matched.v1.p.>`); observers use ephemeral consumers |

Partitioning is fixed at `PARTITIONS = 1024` (`partition.rs`). The job plane keys
by `(graph, region, lane)`, not partition; `topology::partition_of_subject`
parses the trailing `.p.<p>` token so a receiver can reject an envelope whose
declared identity does not match where it landed. Every job/result/output stream
carries a `Nats-Msg-Id` dedup window of 2 minutes (`topology::DUPLICATE_WINDOW`).

## The three control-plane messages

All three are postcard-framed (`bus::Wire` via `postcard_wire!`) and live in
`protocol/`. Their identities (`protocol/ids.rs`) are deterministic, so an
ambiguous publish is retried with byte-identical bytes and the broker
deduplicates it.

| Message | Module | Identity (`Nats-Msg-Id`) |
|---|---|---|
| `SolveJob` | `protocol/job.rs` | `JobId` = `sha256("routers.solve-job.v1" ∥ postcard(JobIdentity))[..16]`, 32 lowercase hex |
| `SolveResult` | `protocol/result.rs` | echoes the `JobId` of the job it answers |
| `CommittedOutput` | `protocol/output.rs` | `OutputId` = `sha256(job_id ∥ "output")[..16]` |

A `JobIdentity` is everything that makes a solve context unique: schema version,
vehicle id, the head `ObservationId`, the optional committed base
(`revision` + `segment`), graph version, and region. A `SolveResult` echoes the
identity so the owner validates without a lookup; its `SolveOutcome` is a typed
enum (`Solved`, `Unanchored`, `Disconnected`, `UnsupportedCoverage`,
`VersionMismatch`, `DeadlineExpired`, `Oversized`, `Internal`). A
`CommittedOutput`'s `OutputKind` is `Matched` (with `finalized_through`),
`Retraction`, `Terminal`, or `Reset`.

Identities and header helpers (`protocol/ids.rs`): `SchemaVersion`,
`ObservationId { partition, sequence }`, `Revision`, `SegmentId`, `JobId`,
`GraphVersion`, `RegionId`, `Lane`, `OutputId`; headers `x-routers-schema`,
`x-routers-sent-at-ms`, `x-routers-received-at-ms`, `Nats-Msg-Id`.

## Durable state and the commit sequence

The orchestrator's only persistent memory is Valkey (`store/`). Keys are
hash-tagged `{vehicle:<id>}` so a vehicle's three keys share one slot for atomic
Lua prepare/promote on a future Cluster.

| Key | Holds | TTL |
|---|---|---|
| `{vehicle:<id>}:checkpoint` | `VehicleCheckpoint` (retained `Trip`, revision, segment, `finalized_through`, graph) | idle policy, default 10m |
| `{vehicle:<id>}:prepared` | `PreparedCommit` (exact output bytes + next checkpoint) | none |
| `partition:<p>:prepared` | set of vehicle ids with a staged commit | none (recovery hint) |
| `partition:<p>:frontier` | the partition's completion frontier | none |

A commit (`orchestrator/commit.rs`) is crash-safe because it is idempotent at
every step — **prepare → publish → promote → complete**:

1. **prepare** stages the exact ordered output bytes and the next checkpoint
   atomically (`PrepareOutcome::{Prepared, AlreadyPrepared, Conflict, Busy}`).
   A second prepare for a different output returns `Busy`, which is how a
   prepared commit wins arbitration against a later deadline.
2. **publish** emits each `CommittedOutput` under its own `OutputId` dedup key.
3. **mark_published** records that the output is durable.
4. **promote** installs the checkpoint and deletes the prepared record.
5. **complete** acknowledges the raw and result deliveries and advances the
   frontier.

`Committer::finish_prepared` re-drives a commit a crash left half-done: it
re-publishes the identical bytes (the broker deduplicates) and promotes.

## Orchestrator modules (spec §3 → file)

| Spec module | File |
|---|---|
| Ordered raw reader | `orchestrator/reader.rs` |
| Vehicle state and scheduler | `orchestrator/scheduler.rs`, `orchestrator/worker.rs` |
| Region resolver | `region/resolver.rs` |
| Admission controller | `orchestrator/admission.rs` |
| Job builder and publisher | `orchestrator/dispatch.rs` |
| Result reader and validator | `orchestrator/validate.rs` |
| Commit coordinator | `orchestrator/commit.rs` |
| Completion-frontier tracker | `orchestrator/frontier.rs` |
| Recovery coordinator | `orchestrator/recovery.rs` |
| Deadline and terminal-outcome handler | `orchestrator/deadline.rs` |
| Lifecycle handler | `lifecycle.rs`, `orchestrator/worker.rs` |

## Matcher modules (spec §4 → file)

| Spec module | File |
|---|---|
| Graph bootstrap | `matcher/bootstrap.rs` (+ `region/artifact.rs`, `region/catalog.rs`) |
| Capacity-bound pull loop | `matcher/pull.rs` |
| Job validator | `matcher/validate.rs` |
| Matching engine | `matcher/engine.rs` |
| Result publisher | `matcher/publish.rs` |
| Lifecycle and telemetry | `lifecycle.rs`, `telemetry.rs`, `metrics.rs` |

## Binaries

Run `cargo run -p routers_realtime --bin <bin> -- --help` for the full list.

* **orchestrator** — `--nats` and `--catalog` are required. Partition ownership
  is static: give `--partitions 0-255` directly or derive it from
  `--pod-name`/`--fleet`. Tunables cover retention (`--raw-retention`,
  `--results-retention`, `--output-retention`, `--jobs-ttl`, `--checkpoint-ttl`),
  admission (`--admit-global-jobs`/`-bytes`, `--admit-region-jobs`/`-bytes`),
  continuity (`--gap`, `--jump-distance`), and lifecycle (`--parked-limit`,
  `--idle-ttl`, `--grace`, `--ack-timeout`, `--streams`, `--raw-max-ack-pending`,
  `--raw-ack-wait`, `--valkey`).
* **matcher** — `--nats`, `--catalog`, `--region`, `--shard-dir` required.
  `--slots` bounds concurrent solves (defaults to available parallelism);
  `--ready-file` drops an exec-probe marker; `--max-decoded-bytes` (default 4 MiB)
  caps a job's decoded size; `--search-distance` overrides the candidate
  generator; `--grace` bounds drain.
* **materializer** — `--nats` required. `--valkey` selects the sink,
  `--partitions` narrows the filter so a shard of materializers can divide the
  plane, and `--consumer-name` (default `materializer`) chooses shared vs.
  independent replay.
* **replay** — `--file` and `--nats` required. Replays a dataset through the
  ingress contract at `--speed`/`--loops`/`--lanes`; `--isolated <run>` prefixes
  subjects with `replay.<run>.` so historical jobs never enter the live deadline
  path. `--max-age`/`--max-ahead` relax the ingress freshness bounds for backfill.

## Failure responsibility map (spec §10 → implementation)

| Situation | Responsible | Where |
|---|---|---|
| Job publish outcome unknown | Dispatcher | `orchestrator/dispatch.rs` (retry identical id/bytes; do not advance) |
| Matcher disappears | NATS redelivery + replacement matcher | `topology/jobs.rs` (`max_deliver` 3, `ack_wait`), `matcher/pull.rs` |
| Job never produces a result | Deadline handler | `orchestrator/deadline.rs` (terminal via the commit path) |
| Duplicate, late, or incompatible result | Validator | `orchestrator/validate.rs` (reject/park/quarantine; enforce finality) |
| Output published but owner crashes | Recovery / commit | `orchestrator/recovery.rs`, `orchestrator/commit.rs` (re-publish, promote) |
| Required short recovery state missing | Recovery | `orchestrator/recovery.rs` (`Reset{StateLost}`, new segment) |
| Materializer restarts or lags | Materializer | `materializer/consumer.rs`, `materializer/sink.rs` (idempotent apply) |
| New graph fails loading | Matcher bootstrap + health | `matcher/bootstrap.rs`, `lifecycle.rs` (never pull while unready) |
| Region becomes hot | KEDA/HPA | out of scope (see below) |
| Orchestrator ownership ambiguous | Ownership-transfer procedure | out of scope (deferred) |

## Deliberately out of scope

This crate is application code only. It does **not** contain: the resource
generator / catalog-driven deployment pipeline, KEDA/HPA autoscaling, or
monitoring dashboards (all IaC); ownership-transfer / failover tooling; and the
BigQuery raw archive or matched sink. Metrics are emitted over OTLP
(`metrics.rs`, `telemetry.rs`) for external collection, but the collectors,
scalers, and dashboards live elsewhere.

## Open decisions carried from the spec (§11)

* Runtime region-assignment manager — deferred; static disjoint ownership for now.
* Autonomous orchestrator failover/reassignment — needs enforced commit fencing;
  the initial non-overlapping ownership transfer has an availability limit.
* Raw `Limits` journal and prepared-state durability — retention/failure policy
  still to be pinned down.
* Live serving and the materializer sink — the downstream read model, transport,
  consumer retention, and persistence strategy are still open.
* Region boundary compatibility — static overlap and a validated fallback must be
  defined; sub-2km trips are not a worst-case coverage guarantee.
* Timings and memory — freshness/queue ceilings and recovery horizons are tunable
  defaults, not validated bounds.

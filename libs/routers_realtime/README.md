# `routers_realtime`

`routers_realtime` is the durable real-time map-matching pipeline. It turns raw vehicle observations into revisable, committed matched output using NATS JetStream for transport and Valkey for per-vehicle checkpoint and served-view storage. Processes communicate only through the broker; adapter traits let the same behaviour run against in-memory fakes in tests.

## Architecture

```text
replay / ingress
      | raw journal (partitioned by vehicle)
      v
orchestrator -- solve jobs (per graph + region) --> matcher
      ^                                             |
      |       solve results (partitioned)           |
      +---------------------------------------------+
      |
      +-- committed output (partitioned) --> materializer --> Valkey served view
      |
      +-- Valkey checkpoint store (checkpoint, prepared commit, frontier)
```

| Component | Entrypoint and implementation |
| --- | --- |
| Ingress and replay | `bin/replay.rs`, [`ingress`], [`topology::raw`] |
| Partition owner / orchestrator | `bin/orchestrator.rs`, [`orchestrator`] |
| Regional matcher | `bin/matcher.rs`, [`matcher`] |
| Output materializer | `bin/materializer.rs`, [`materializer`] |
| JetStream adapters | [`bus::jetstream`] and [`topology`] |
| Durable checkpoint store | [`store::checkpoint`] and [`store::valkey`] |
| Served Valkey view | [`materializer::valkey`] |
| Wire messages and stable identifiers | [`protocol`] and [`event`] |
| Region catalog and graph artifacts | [`region`] |

### JetStream planes

- Raw observations use `events.raw.p.<partition>` and `EVENTS-RAW-<index>`. They use limits retention: acknowledgements advance consumers but do not delete the replay journal.
- Solve jobs use `solve.v1.g.<graph>.r.<region>.q.<lane>` and one work-queue stream per region. Matcher replicas for the same graph and region share a durable consumer, so the broker load-balances each job once.
- Results use `solve-result.v1.p.<partition>` in `SOLVE-RESULTS` and are read by the owning orchestrator partition.
- Committed output uses `events.matched.v1.p.<partition>` in `EVENTS-MATCHED`; materializers consume it durably and observers may tail it.

Subject names, stream names, partition routing, postcard field order, and protocol identifiers are compatibility boundaries. Change them only with a coordinated data migration.

## Durability and state machines

An orchestrator has exclusive ownership of every vehicle partition it runs. It rebuilds each partition before opening its raw consumer, then resumes strictly after the durable raw frontier. A vehicle checkpoint contains the retained match state, revision, segment, finality watermark, graph, schema, and region.

A commit is a recoverable three-step state machine:

1. `prepare` compare-and-stages the exact output bytes and next checkpoint in Valkey. The expected prior revision prevents two commits winning.
2. `publish` sends those exact bytes with the output ID as `Nats-Msg-Id`. Retrying after an ambiguous broker response is safe because JetStream deduplicates the message.
3. `promote` installs the next checkpoint and advances the partition frontier.

The prepared record is either `Prepared` (publication still required) or `Published` (only promotion remains) and has no TTL. Recovery re-drives either state, so a crash at any point cannot silently lose a committed output. The materializer persists output before acknowledging it; its merge function is idempotent, so redelivery is safe.

Continuity decisions and materialized history also use explicit enums: a matched result continues or resets a segment, terminal outcomes record why a segment closed, and finalized layers cannot be rewritten. The ingestion path does not impose a fleet-wide wall-clock “future” or “too old” classification: replayed distributed events remain valid input; continuity is evaluated per vehicle by its ordered state.

## Local development

From the workspace root, use the focused checks:

```sh
cargo fmt --check
cargo check -p routers_realtime --all-targets
cargo test -p routers_realtime
cargo doc -p routers_realtime --no-deps
```

Run an individual binary's argument validation with, for example:

```sh
cargo run -p routers_realtime --bin orchestrator -- --help
cargo run -p routers_realtime --bin matcher -- --help
cargo run -p routers_realtime --bin materializer -- --help
cargo run -p routers_realtime --bin replay -- --help
```

The normal test suite uses the memory bus and memory checkpoint store; it does not require a running NATS or Valkey. Running the binaries requires a NATS server with JetStream enabled, reachable Valkey primaries, a valid region catalog, and matcher shard artifacts.

## Deployment and configuration

Each binary accepts its main connection values from flags or the corresponding uppercase environment variables emitted by Clap (for example `--nats` / `NATS` and `--valkey` / `VALKEY`). NATS and Valkey URLs may carry credentials. They are parsed as [`secret::SecretUrl`]: diagnostics show only scheme, host, and port. Valkey endpoints use `stable-id=URL` (for example `primary=redis://valkey:6379`); rendezvous placement hashes the stable ID, so rotating a URL or its credentials does not remap vehicles. Plaintext URL access is confined to client construction. Do not place these URLs in command output, support bundles, or hand-written logs.

The orchestrator reconciles raw, job, result, and output streams before it starts workers. All orchestrator replicas must agree on the partition mapping, raw-stream count, catalog, retention, and the complete Valkey checkpoint fleet. Each partition must be owned by exactly one live orchestrator. StatefulSet ordinal assignment (`--pod-name` plus `--fleet`) derives contiguous ownership; an explicit `--partitions` range is available for controlled deployments.

Matcher replicas are scoped to one catalog region and its pinned graph. Their shared `(graph, region)` durable consumer is intentional: matching replicas with the same configuration share work. Materializer replicas similarly share `--consumer-name` when they are intended to share work; choose distinct names to replay independently. Every process that reads or writes a Valkey fleet must receive the same unordered set of stable node IDs; connection URLs may rotate independently.

Retention is an operational recovery budget, not just a storage cost. Keep raw retention longer than the maximum expected outage and rewind. Keep results and output long enough for their consumers to recover. Do not alter raw stream count, names, subject prefixes, protocol schema, or Valkey endpoint identity in place; treat each as a migration.

## Invariants

- One partition owner advances one partition frontier; raw revisions are its stream sequence and preserve each vehicle's routing.
- A checkpoint promotion never precedes durable output publication.
- Retried jobs, results, and committed outputs retain their IDs and bytes so broker deduplication and store operations stay idempotent.
- A matcher acknowledges a job only after its result has been published.
- A materializer acknowledges output only after its Valkey merge has completed.
- Finalized layers are immutable; a later revision can revise only permitted history in the same vehicle segment.
- The checkpoint and served-view Valkey fleets are separate concerns; endpoint reordering and URL rotation are safe, but changing the set of stable node IDs remaps keys and requires planning.

[`ingress`]: https://docs.rs/routers_realtime/latest/routers_realtime/ingress/index.html
[`orchestrator`]: https://docs.rs/routers_realtime/latest/routers_realtime/orchestrator/index.html
[`matcher`]: https://docs.rs/routers_realtime/latest/routers_realtime/matcher/index.html
[`materializer`]: https://docs.rs/routers_realtime/latest/routers_realtime/materializer/index.html
[`topology`]: https://docs.rs/routers_realtime/latest/routers_realtime/topology/index.html
[`topology::raw`]: https://docs.rs/routers_realtime/latest/routers_realtime/topology/raw/index.html
[`bus::jetstream`]: https://docs.rs/routers_realtime/latest/routers_realtime/bus/jetstream/index.html
[`store::checkpoint`]: https://docs.rs/routers_realtime/latest/routers_realtime/store/checkpoint/index.html
[`store::valkey`]: https://docs.rs/routers_realtime/latest/routers_realtime/store/valkey/index.html
[`materializer::valkey`]: https://docs.rs/routers_realtime/latest/routers_realtime/materializer/valkey/index.html
[`protocol`]: https://docs.rs/routers_realtime/latest/routers_realtime/protocol/index.html
[`event`]: https://docs.rs/routers_realtime/latest/routers_realtime/event/index.html
[`region`]: https://docs.rs/routers_realtime/latest/routers_realtime/region/index.html
[`secret::SecretUrl`]: https://docs.rs/routers_realtime/latest/routers_realtime/secret/struct.SecretUrl.html

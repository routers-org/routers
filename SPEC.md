# Routers Realtime Service — Specification

## Overview

A GKE-hosted distributed map-matching pipeline that ingests real-time GPS events,
enriches them with historical context, matches each position to the road network,
and emits an ordered stream of matched positions keyed by vehicle ID.

The design is **sans-IO throughout**: all matching logic is pure data-in/data-out,
making load testing and unit testing trivially achievable without a live network.

---

## System Components

```
                     ┌────────────────────────┐
  GPS events         │  Orchestrator (n pods)  │
 ──RabbitMQ──►       │  - Reads AMQP stream    │
                     │  - Maintains hot history │
                     │    in Valkey per vehicle │
                     │  - Computes target shard │
                     │  - Publishes MatchContext│
                     └──────────┬──────────────┘
                                │ NATS JetStream
                                │ match.{shard_id}
                     ┌──────────▼──────────────┐
                     │   Matcher pods (sharded)  │
                     │  - Owns one geohash zone  │
                     │  - Loads shard + halo     │
                     │    neighbours from local  │
                     │    filesystem on startup  │
                     │  - Calls Match::match()   │
                     │  - Emits MatchResult      │
                     └──────────┬──────────────┘
                                │ NATS JetStream
                                │ matched.positions
                                ▼
                          (downstream consumers)
```

---

## Data Flow

1. A vehicle GPS event arrives on a RabbitMQ topic.
2. The **orchestrator** receives the event and:
   - Looks up the vehicle's position history in **Valkey** (Redis Streams).
   - Appends the new position and retrieves the recent history.
   - Filters history to positions from the **last two distinct shard zones**
     the vehicle has been in, keeping only actionable context for the target pod.
   - Computes the target geohash (precision 5 by default, configurable).
   - Serialises a `MatchContext` packet (postcard) and publishes to NATS
     JetStream on subject `match.{shard_id}`.
3. The **matcher pod** responsible for that shard:
   - Receives the `MatchContext` via a durable JetStream pull consumer.
   - Assembles a `LineString` from `history + current` in chronological order.
   - Calls the existing `Match::match()` trait (HMM-based map matching).
   - Serialises a `MatchResult` (vehicle ID, resolved timestamp, matched coord)
     and publishes to `matched.positions`.
   - Acknowledges the JetStream message.

---

## History Policy

Positions are stored in Valkey as a per-vehicle Redis Stream:

```
key:  vehicle:{id}:positions
fields per entry:
  shard  →  postcard-encoded ShardId
  pos    →  postcard-encoded Position { coord: Point, timestamp_ms: u64 }
```

On each new event, the orchestrator:
- `XADD … MAXLEN ~ 200` (bounded stream, ~200 entries)
- `XREVRANGE … COUNT 200` (newest-to-oldest)

Application-level filter (`history::filter_history`):
- Walk entries newest-to-oldest.
- Track distinct shard IDs seen.
- Collect positions from the first two distinct shard IDs encountered.
- Reverse the result (chronological order for the matcher).

This ensures the matcher never receives positions from shard zones for which it
has no map data — anything beyond its owned zone plus its immediate halo
neighbours would produce a failed or incorrect match.

---

## Shard Assignment

Matcher pods determine their owned geohash at startup via the `ShardAssignment`
trait:

| Build mode | Implementation | Mechanism |
|---|---|---|
| `debug` | `EnvAssignment` | `OWNED_SHARD` environment variable |
| `release` | `NatsKvAssignment` | Atomic CAS lease in NATS KV bucket |

Both implement a single `trait ShardAssignment<S: ShardId>`.

NATS JetStream's durable pull consumers are named `matchers-{shard_id}`.
Multiple pods may join the same consumer group for a shard, with NATS
distributing messages across them (exactly-once delivery per message within
the group).

KEDA watches the JetStream consumer lag per subject partition and scales the
`StatefulSet` up or down accordingly.

---

## Shard Data Loading

On startup, each matcher pod:

1. Determines its owned shard.
2. Computes the 9-cell neighbourhood (`SelectionMode::OwnedAndNeighbours`).
3. Fetches each shard file from the local filesystem via `FileFetcher`:
   - File naming: `{geohash}.shard.rt`
   - Directory: `SHARD_DIR` environment variable.
4. Loads each shard via `ShardLoader`, caching in memory.
5. Combines all shards into a `MultiShardNetwork`.

**Future**: A node-level DaemonSet will pre-warm shard files to local NVMe, and
the pod will simply `mmap` them via `FileFetcher` with a `hostPath` mount.
The code is written so this transition requires only changing `SHARD_DIR` to the
pre-warmed path — no refactoring.

---

## I/O Abstractions

All input and output boundaries use standard `futures_util` `Stream` / `Sink`
primitives:

- **Input**: any `impl Stream<Item = RawEvent>` — currently an AMQP `Topic`
  mapped through a parsing layer.
- **Output (orchestrator → matcher)**: `impl Sink<MatchContext<S>>` backed by a
  NATS JetStream publish.
- **Output (matcher → consumers)**: `impl Sink<MatchResult>` backed by NATS.

Adding a NATS input source in place of RabbitMQ is a single `impl Stream`
addition in `routers_realtime::nats`.

---

## Key Types

```
RawEvent          { vehicle_id, coord: Point, timestamp_ms }
Position          { coord: Point, timestamp_ms }
MatchContext<S>   { vehicle_id, resolved_at_ms, history: Vec<Position>,
                    current: Position, target_shard: S }
MatchResult       { vehicle_id, resolved_at_ms, coord: Point }
```

`S` is any `ShardId` from `routers_shard`. The default concrete type in
production binaries is `Geohash` (precision 5).

`resolved_at_ms` is a wall-clock millisecond timestamp assigned by the
orchestrator at intake — independent of the vehicle's GPS timestamp — so
consumers can resolve ordering without trusting device clocks.

---

## Output Contract

Subject: `matched.positions`  
Encoding: `postcard`  
Type: `MatchResult { vehicle_id: String, resolved_at_ms: u64, coord: Point }`

Each message is an atomic matched position for one vehicle at one point in time.
`vehicle_id` is the stable routing key; `resolved_at_ms` is the ordering basis.

---

## Infrastructure (GKE + Pulumi)

Located in `pulumi/`.

| Resource | Kind |
|---|---|
| GKE Autopilot cluster | `gcp.container.Cluster` |
| GCS bucket (shard files) | `gcp.storage.Bucket` |
| Artifact Registry | `gcp.artifactregistry.Repository` |
| NATS JetStream (3-node) | Helm chart via `kubernetes.helm.v4.Chart` |
| Valkey (Redis-compat) | `kubernetes.apps.v1.StatefulSet` |
| KEDA | Helm chart |
| Orchestrator | `kubernetes.apps.v1.Deployment` |
| Matcher | `kubernetes.apps.v1.StatefulSet` |
| Workload Identity | `gcp.serviceaccount.IAMMember` |

---

## Containers

| Image | Entry crate | Triggered by |
|---|---|---|
| `routers-orchestrator` | `routers_realtime` bin `orchestrator` | release tag |
| `routers-matcher` | `routers_realtime` bin `matcher` | release tag |
| `routers-rpc` | `routers_rpc` bin `server` | release tag |

Built in `auto-release.yml` after `release-plz-release` succeeds, pushed to
Artifact Registry tagged with the crate version.

---

## Environment Variables

### Orchestrator

| Variable | Default | Purpose |
|---|---|---|
| `RABBITMQ_URL` | `amqp://127.0.0.1:5672/%2f` | AMQP broker |
| `RABBITMQ_EXCHANGE` | `amqprs.example` | Exchange name |
| `RABBITMQ_QUEUE` | `queue` | Queue name |
| `RABBITMQ_ROUTING_KEY` | `amq.topic` | Routing key |
| `NATS_URL` | `nats://127.0.0.1:4222` | NATS server |
| `VALKEY_URL` | `redis://127.0.0.1:6379` | Valkey/Redis |
| `SHARD_PRECISION` | `5` | Geohash precision |
| `HISTORY_MAX_POINTS` | `50` | Max history positions per `MatchContext`; increase for better HMM accuracy |
| `HISTORY_MAX_AGE_SECS` | `300` | Drop positions older than this many seconds |
| `VALKEY_MAX_LEN` | `500` | Valkey stream MAXLEN per vehicle (should be ≥ `HISTORY_MAX_POINTS`) |

### Matcher

| Variable | Default | Purpose |
|---|---|---|
| `OWNED_SHARD` | *(required in debug)* | Geohash string e.g. `r3gx2` |
| `SHARD_DIR` | `./shards` | Directory containing `.shard.rt` files |
| `SHARD_PRECISION` | `5` | Geohash precision |
| `NATS_URL` | `nats://127.0.0.1:4222` | NATS server |

### RPC Server

| Variable | Default | Purpose |
|---|---|---|
| `SHARD_DIR` | `./shards` | Directory containing `.shard.rt` files |
| `SHARD_ID` | *(required)* | Geohash string for the shard to serve |
| `SHARD_PRECISION` | `5` | Geohash precision |
| `RPC_ADDR` | `[::1]:9001` | gRPC listen address |

---

## Non-Goals (this phase)

- GCS shard file fetching (local FS only; DaemonSet pre-warm deferred).
- NATS KV lease acquisition (stub only; env-based assignment used in debug).
- Output consumer implementations (downstream of `matched.positions`).
- Shard generation pipeline (pre-computed shards sourced externally).
- Horizontal autoscaling configuration tuning.

# Routers Monitor — Visualiser Specification

## Overview

A second binary (`monitor`) added to the `routers_viewer` crate. It subscribes to two
NATS subjects, correlates raw GPS positions with matched outputs per vehicle, and renders
rolling per-vehicle traces on a live walkers map — showing both the raw GPS path and the
snapped matched path side-by-side so the correctness of each matching decision is
immediately legible.

Replay is driven externally (`cargo run --example replay -- --speed 2`); there is no
replay UI in the monitor.

---

## Crate Changes

`routers_viewer` gains a second `[[bin]]` entry (`src/bin/monitor.rs`) alongside the
existing interactive viewer binary. New dependencies added to `routers_viewer/Cargo.toml`:

```toml
async-nats       = { workspace = true }
postcard         = { workspace = true }
routers_realtime = { workspace = true }
routers_shard    = { workspace = true }
seahash          = "4"
```

`routers_realtime` is a dev-only dependency (`publish = false` on both crates), which is
acceptable. If its transitive dependency graph becomes a problem, the `context.rs` types
can be extracted into a minimal `routers_realtime_types` crate without changing the
monitor's interface.

---

## NATS Subject Strategy

Two subjects cover the full pipeline without interfering with production consumers.

### 1. Raw GPS — `match.>` (JetStream, ephemeral ordered consumer)

The orchestrator publishes each `MatchContext<Geohash>` to `match.{shard_id}`. The MATCH
JetStream stream already covers `match.>`. The monitor creates an **ephemeral ordered
push consumer** with `deliver_policy: New` and filter subject `match.>`, receiving all
new MatchContext messages across every shard in one subscription. Because it is ephemeral
and ordered (not durable), it never competes with the `matchers-{shard}` pull consumer
groups.

Useful fields from `MatchContext<Geohash>`:

| Field | Purpose |
|---|---|
| `vehicle_id` + `resolved_at_ms` | Correlation key |
| `current.coord` | Raw GPS position for this event |
| `target_shard` | Which geohash cell is processing this event |

`history` is intentionally ignored in v1 — the current position is sufficient for trace
validation, and replaying history would double-draw older fixes already in the store.

### 2. Matched output — `matched.positions` (core NATS, plain subscribe)

The matcher publishes `MatchResult` via `async_nats::Client::publish()` (not JetStream).
The monitor subscribes with a plain `client.subscribe("matched.positions")` — no stream
or consumer configuration required.

Useful fields from `MatchResult`:

| Field | Purpose |
|---|---|
| `vehicle_id` + `resolved_at_ms` | Correlation key |
| `coord` | Snapped matched coordinate |
| `outcome` | `Success` / `NoCandidate` / `Error` |

### Correlation

Both messages share `(vehicle_id, resolved_at_ms)` — `resolved_at_ms` is assigned by
the orchestrator at intake so it is stable across the pipeline. The monitor maintains a
short-lived pending map that is drained each render frame:

```
PendingMap: HashMap<(vehicle_id, resolved_at_ms), PendingEntry>

PendingEntry {
    context:     Option<MatchContext<Geohash>>,
    result:      Option<MatchResult>,
    inserted_at: Instant,
}
```

When both sides arrive the entry is joined into a `VehicleFix` and committed to the
trace store. Entries older than 5 seconds with only one side present are evicted — this
handles dropped messages without leaking memory. In practice the gap between the two
messages should be well under 500 ms.

---

## Architecture

```
NATS match.>                     NATS matched.positions
  │  (JetStream ephemeral         │  (core subscribe)
  │   ordered consumer)           │
  │  MatchContext<Geohash>        │  MatchResult
  └──────────────┬────────────────┘
                 │
         context_task / result_task  (tokio, inside held Runtime)
                 │  std::sync::mpsc::Sender<InboundMessage>
                 ▼
          VehicleTraceStore  (owned by egui App, drained each frame)
          ┌────────────────────────────────────────────────┐
          │  pending: HashMap<CorrelKey, PendingEntry>     │
          │  traces:  HashMap<VehicleId, VecDeque<Fix>>    │  (max 50 fixes)
          │  stats:   EventRate, OutcomeCounts             │
          └────────────────────────────────────────────────┘
                 │
          egui render loop  (30 fps)
          ├── Map  (walkers + TracePlugin)
          └── StatsPanel
```

---

## Key Types

```rust
struct VehicleFix {
    vehicle_id:     String,
    resolved_at_ms: u64,
    raw_coord:      Point,         // MatchContext.current.coord
    matched_coord:  Option<Point>, // None when outcome != Success
    outcome:        MatchOutcome,
    shard:          Geohash,
}

struct VehicleTraceStore {
    pending: HashMap<(String, u64), PendingEntry>,
    traces:  HashMap<String, VecDeque<VehicleFix>>,
    stats:   StoreStats,
}

struct StoreStats {
    events_per_sec:   f32,   // rolling 1-second bucket
    outcome_success:  u64,
    outcome_no_cand:  u64,
    outcome_error:    u64,
}
```

---

## UI Layout

```
┌──────────────────────────────────────────┬─────────────────────┐
│                                          │  Vehicles:   312    │
│  [walkers tile map]                      │  Events/s:   847    │
│                                          │  Outcomes:          │
│  Per-vehicle overlay (viewport-culled):  │   ✓  98.4% matched  │
│                                          │   ○   1.2% no cand. │
│  · Thin grey polyline  — raw GPS trace   │   ✗   0.4% error    │
│  · Coloured polyline   — matched trace   ├─────────────────────┤
│  · Small open circle   — latest raw fix  │  Connection         │
│  · Filled circle       — latest match    │  NATS: [_________]  │
│  · Thin connector line — raw → matched   │  [Connect]          │
│    per fix (visualises snap distance)    └─────────────────────┘
│
│  Vehicle colour = SeaHash(vehicle_id) → HSL hue
│  Opacity fades toward tail of each trace
└──────────────────────────────────────────────────────────────────
```

---

## TracePlugin

A new `walkers::Plugin` implementation. `LineStringPlugin` from the interactive viewer is
not reused — it is designed for single confirmed matches, not rolling multi-vehicle
traces.

On each frame the plugin receives a snapshot reference to `VehicleTraceStore`. For each
vehicle whose most recent fix falls within the current map bounding box:

1. Project all fix coordinates via `Projector::project`
2. Draw raw GPS polyline — translucent grey, 1 px, opacity fading toward tail
3. Draw matched polyline — vehicle colour, 2 px, Success fixes only, same fade
4. For each fix: draw a thin dashed connector from raw to matched coord
5. Draw the most recent raw coord as a small open circle (3 px radius)
6. Draw the most recent matched coord as a filled circle in vehicle colour (4 px radius)

Viewport culling is applied before step 1 — only vehicles whose last `raw_coord` or
`matched_coord` lies inside the current tile bbox are processed. At geohash precision 5
(~5 km cells) a typical urban viewport contains at most a few hundred active vehicles.

---

## Reused from `routers_viewer`

| Component | File | Reuse |
|---|---|---|
| `Component` trait | `src/utils/component.rs` | StatsPanel sidebar |
| `ColourScheme` + light/dark impls | `src/utils/colour.rs` | Map background, text, UI chrome |
| walkers `Map` wrapper | `src/components/map.rs` | Direct reuse |
| `Stack` layout helper | `src/components/stack.rs` | Sidebar layout |

`MatchData`, `MatchLayer`, `CandidatesPlugin`, `ChosenPathPlugin`, and `DrawPlugin` are
specific to the interactive solver and are not used here.

---

## Async ↔ egui Bridge

A `tokio::Runtime` is held inside the `App` struct (same approach as `routers_viewer`'s
background tile fetching). Two tasks run inside it:

```rust
// Task 1: JetStream ephemeral ordered consumer on match.>
async fn context_task(js: JetStream, tx: SyncSender<InboundMessage>) { … }

// Task 2: core NATS subscribe on matched.positions
async fn result_task(nc: Client, tx: SyncSender<InboundMessage>) { … }
```

Both send into a single `std::sync::mpsc::SyncSender<InboundMessage>` (bounded, capacity
8192). The render loop calls `rx.try_iter().take(2000)` each frame to drain pending
messages into the store, then renders. Repaint is driven by
`ctx.request_repaint_after(Duration::from_millis(33))` (30 fps cap).

---

## Anticipated Challenges

**Ordered consumer on a hot stream**
An ephemeral ordered consumer re-delivers on sequence gaps. At 25k+ evt/s the MATCH
stream is very active. If the monitor falls behind, the consumer will lag but will not
block or starve production matchers. The 2 000-message-per-frame drain at 30 fps gives
a sustainable budget of ~60 k messages/s, which should stay ahead of the stream at
normal event rates.

**Correlation timing**
MatchContext and MatchResult for the same key can arrive in either order with up to
~500 ms between them (matcher solve latency + NATS delivery). The 5-second eviction
window is generous. At high load, if the matcher queues deeply, correlation gaps will
widen — they will show as raw GPS dots with no matched companion, which is itself a
useful diagnostic signal rather than a silent failure.

**`routers_realtime` in `routers_viewer`'s dependency graph**
This pulls in the full realtime crate (RabbitMQ client, Valkey client, NATS, postcard).
Both crates are `publish = false` and this is a dev binary, so it is acceptable for now.
Extraction to a `routers_realtime_types` crate remains the clean long-term path if
compile times become a problem.

**Colour distinguishability at scale**
Hue derived from `SeaHash(vehicle_id) % 360` clusters badly with many vehicles. Use a
fixed 32-hue perceptual palette (evenly spaced in OKLCH) indexed by `SeaHash % 32`
instead, accepting rare collisions in exchange for consistently distinct colours.

---

## Non-Goals (this phase)

- History trace from `MatchContext.history`
- Replay UI (run `cargo run --example replay` from a terminal)
- WASM build (async-nats has no WASM target without a WebSocket bridge)
- Recording traces to disk
- Shard boundary grid overlay
- Per-vehicle detail panel (click to inspect individual fixes)

# Incremental matching (streaming HMM)

Per-vehicle matcher state held across events, so each event extends an
already-solved transition graph by one layer instead of re-solving from
scratch.

> ⚠ This requires a **new solver variant** in the routers crate — not a
> refactor of the existing one. See §Phase 1 for scope.

## Why

The matcher today calls `Match::r#match(linestring, opts)` on every event
with `linestring = history + current` (6 points after `HISTORY_MAX_POINTS`
was reduced to 5). Each call:

- generates candidates for every layer,
- runs a bounded Dijkstra `reach()` between every pair of consecutive layers
  (`precompute_forward.rs:200–230`),
- A*-searches over a synthetic-start/end DAG built from all those reaches,
- discards every byte of intermediate state on return.

Five of the six layers were already matched on the previous event. We're
paying ~5× the work that's actually new per event. That's the working
hypothesis for the latency creep observed under sustained load.

> 🔍 **Hypothesis, not proven.** Before committing to the rewrite, we
> profile the current solver under controlled load (see Phase 0). If the
> bottleneck turns out to be predicate-cache cold-misses or
> `scc::HashMap` contention rather than repeated-layer work, this design
> needs revisiting.

## Design

### Per-vehicle state

```rust
pub struct MatchState<E: Entry> {
    /// Frontier set: one entry per candidate at the most-recent layer.
    /// All identifiers here are stable across solves — `ThinEdge<E>`
    /// carries the underlying graph edge id, not a per-solve positional
    /// candidate id (those don't survive across solves).
    frontier: Vec<FrontierNode<E>>,
    /// Most-recent matched (snapped) coord — used to emit incremental
    /// MatchRoute polyline segments without recomputing the full path.
    last_matched: Point,
    /// GPS timestamp of the event that produced this frontier.
    last_event_ms: u64,
}

struct FrontierNode<E> {
    /// The graph edge this candidate sits on.
    edge: ThinEdge<E>,
    /// Snapped position on that edge.
    snapped: Point,
    /// Cumulative cost along the best path that reached this frontier
    /// node. Stable units (sum of emissions + transitions). Resets on
    /// cold-start.
    cum_cost: f64,
    /// Predecessor: the underlying edge id of the frontier node from
    /// which this candidate's best path arrived. `None` for cold-start
    /// frontier nodes (first event after gate).
    back_edge: Option<E>,
}
```

Memory: dense urban shards routinely yield 30–50 candidates per layer, not
10. Worst-case per-vehicle: 50 × (~120 bytes per `FrontierNode` including
`ThinEdge` payload) + cache-line / map overhead ≈ **8 KB worst case**,
~1–2 KB typical. At `MATCH_STATE_CAP=100_000`: ~800 MB worst-case,
~150 MB typical. **Measure before defaulting.** If worst-case is real,
default cap drops to 20_000.

### Per-event algorithm

1. **Lookup** `state_cache.get(vehicle_id)` (concurrent events for the
   same vehicle resolved via optimistic write-wins-by-timestamp — see
   §Concurrency below).
2. **Cold-start gate** — fall through to a full `r#match` solve over
   `ctx.history + ctx.current` if any of:
   - state missing (first event for this vehicle on this pod, or after
     LRU/TTL eviction),
   - `now - state.last_event_ms > MATCH_STATE_TTL_SECS`,
   - the warm-step computation returned an empty frontier on the
     previous event (graceful re-anchor — see §Empty extension),
   - the warm-step returned a `MatchError` (resilience fallback). The
     matcher publishes `MatchOutcome::Error` for the current event AND
     evicts the state entry so the *next* event runs cold-start with
     fresh history. We never block on an error: emit, evict, move on.

   After the cold-start solve, snapshot the final layer's reachable set
   and persist as the new `MatchState`. **Cold-start fallback ALWAYS runs
   the same solver as today** — no behavioural regression on cold path.
3. **Warm step** (the common path):
   1. `StandardGenerator` generates candidates for `ctx.current` only.
   2. For every new candidate `c_new`, scan the previous frontier and
      compute `reach(prev → c_new)` using the new streaming-solver's
      `reach` primitive.
   3. New frontier entry:
      `cum_cost = min_over_prev( prev.cum_cost + transition_cost + emission(c_new) )`,
      back-pointer set to the minimising `prev.edge.id`.
   4. **Empty extension guard**: if no new candidate has any reachable
      predecessor (tunnel exit, off-network GPS spike, sparse network
      near a shard boundary), emit `MatchOutcome::NoCandidate` AND mark
      the state so the next event takes the cold-start path. Cold-start
      with retained `ctx.history` re-anchors cleanly.
   5. Pick global argmin → emit:
      - `MatchResult { coord = argmin_snapped }`. Note: this differs from
        today's matcher which emits the snapped point at the end of the
        best *full path*. With a single back-pointer hop on warm-step,
        the new argmin is equivalent in the common case (Markov property
        holds for emission + pairwise transition); see §Validation for
        empirical check.
      - `MatchRoute { polyline = edge segment(s) from state.last_matched
        → new snapped }`. Option (a) — incremental segment, not the full
        window's route.
   6. Replace state with the new frontier, refresh `last_event_ms`.

### Concurrency

NATS redelivery, the orchestrator's retry path, and bursty events for
the same vehicle can produce concurrent in-flight events for the same
`vehicle_id`. Two options:
- (a) per-vehicle `tokio::sync::Mutex` — clean, head-of-line for that
  vehicle.
- (b) **optimistic write-wins-by-timestamp** under a per-entry
  `RwLock`: the warm step takes a `write()` lock, re-checks
  `resolved_at_ms > state.last_event_ms`, and only commits if its event
  is the newest seen. Out-of-order redeliveries discard without
  touching state.

**Decision**: (b). Out-of-order events for the same vehicle are
information-poor (the orchestrator already filtered history by
timestamp), and a per-vehicle mutex head-of-lines a hot vehicle's burst
when only the newest event matters. The RwLock contention window is
the warm-step write (microseconds), not the full solve.

### State cache

`moka::sync::Cache<VehicleId, Arc<RwLock<MatchState<E>>>>` with both
`max_capacity(MATCH_STATE_CAP)` and `time_to_idle(MATCH_STATE_TTL_SECS)`.
moka does eviction at insert/access time (no scanning task needed) and
gives correct LRU + TTL semantics out of the box. `Arc<RwLock<…>>` per
entry so the read/write inside the warm step doesn't have to remove and
re-insert.

### TTL vs history-window alignment

`MATCH_STATE_TTL_SECS = 1800` (30 min). To make sure cold-start always
has enough history when a state is evicted, `HISTORY_MAX_AGE_SECS` is
**bumped from 300 → 1800** to match. Cost: orchestrator's
`MemoryStore` keeps more entries per vehicle in RAM. With
`VALKEY_MAX_LEN=500` already absorbing 6× of those entries, this is
cheap; revisit if profiling shows orchestrator memory pressure.

`HISTORY_MAX_POINTS` stays at 5. The orchestrator's `filter_history`
(history.rs) caps both *count* (`max_points`) and *age* (`max_age_ms`).
Bumping `max_age_ms` only extends the age cap — the point cap still
trims to 5. So cold-start always solves a 6-point linestring (5
history + 1 current); the only thing that changes is the maximum
elapsed time those 6 points may span (now up to 30 min for a slowly-
reporting vehicle). Cold-start cost is bounded by point count, not
by time-span.

### Shard handover

The matcher subscribes only to `match.{owned_shard}` (matcher.rs:138),
so a vehicle whose `ctx.target_shard` differs from the matcher's owned
shard would never have reached it in the first place — the `last_shard`
field and its mismatch check from the earlier draft were **redundant**
and have been removed.

Real handover behaviour: as a vehicle crosses A→B, A's matcher stops
seeing it (its state ages out via TTL on the next eviction tick), and
B's matcher cold-starts on first event. Border-straddling vehicles
A→B→A pay multiple cold-starts. Acceptable — the cost is bounded and
the population of straddling vehicles is small relative to the steady
fleet inside a cell.

### API additions (routers crate)

- **New solver variant** alongside `PrecomputeForwardSolver` /
  `SelectiveForwardSolver`:
  ```rust
  pub struct StreamingForwardSolver<E, M, N> { /* ... */ }
  ```
  - Owns its own `PredicateCache` (shared across warm steps; a fresh
    cache is created per cold-start).
  - Implements a new method `step(prev_frontier, new_layer_candidates,
    ctx) -> Result<Frontier<E>, MatchError>` that produces the next
    frontier without building or A*-ing a transition DAG.
  - Implements the existing `Solver` trait for cold-start by running a
    full-window streaming solve from layer 0.
- **Public types** at the routers crate root:
  - `MatchState<E>` (alias for `FrontierWithMeta<E>` if a better name
    emerges).
  - `MatchedFix<E> { snapped: Point, edge: ThinEdge<E>, cost: f64 }` —
    the warm-step's output, slimmer than `RoutedPath`. The matcher
    binary translates this to today's `MatchResult` / `MatchRoute`.
- **New trait method** on `Match`:
  ```rust
  fn match_step(
      &self,
      point: Point,
      ts_ms: u64,
      prior: Option<&MatchState<E>>,
      opts: &MatchOptions<E, M, Self>,
  ) -> Result<(MatchedFix<E>, MatchState<E>), MatchError>;
  ```
  `opts` is the **same** `MatchOptions` as today — `search_distance`,
  `runtime`, `solver`, and `cache` still mean what they mean. `anchor`
  is **ignored** under warm-step because the prior frontier already
  encodes the start point; document that explicitly.

### Matcher binary integration

- `state: Arc<moka::sync::Cache<VehicleId, Arc<RwLock<MatchState>>>>`
  threaded into the message loop.
- Branch cold/warm using the gate above; write state back via the
  per-entry `RwLock` with optimistic write-wins-by-timestamp (see
  §Concurrency).
- Metrics:
  - `routers_match_state_size` (gauge, derived from `Cache::entry_count`).
  - `routers_match_step_total{kind="warm" | "cold"}` (counter).
  - `routers_cold_start_total{reason="missing" | "ttl" | "empty_warm" | "error_fallback"}` (counter).
  - `routers_warm_step_cost_delta_ms` (histogram) — the per-event change
    in `cum_cost`, useful for spotting divergence before it causes
    divergence in match quality.
  - `routers_warm_step_frontier_size` (histogram) — to validate the
    10 vs 30 vs 50 candidate-count assumption.
- All gated behind `MATCH_STATEFUL=1` env flag, default off.

### Validation (Phase 4)

Run stateful matcher in shadow mode alongside the stateless one on the
same NATS subjects. Compare:
- **Divergence rate**: `% of events where snapped edge differs`.
  Acceptable: ≤ 1%.
- **Snapped-coord drift**: `Haversine(stateful_snapped, stateless_snapped)`.
  Acceptable: p99 ≤ 5m, p999 ≤ 25m.
- **Outcome divergence**: `% of events with outcome mismatch
  (Success/NoCandidate/Error)`. Acceptable: ≤ 0.5%.

Drift exceeding these thresholds blocks default-on rollout.

## Phases

### Phase 0 — Profile current matcher [✅ complete, 2026-06-12]

**Verdict: proceed.** Profile gate cleared comfortably (83% avg savings
available vs 40% threshold).

**Instrumentation.** Added two env-gated knobs to
`PrecomputeForwardSolver::solve`:
- `SOLVER_PROFILE_SAMPLE_N=N` — log per-stage timings (gen / astar / total)
  every Nth solve.
- `SOLVER_PROFILE_PER_LAYER=1` — serialise the outer layer iter (inner
  per-candidate iter stays parallel) so each layer-transition's reach
  work is attributed separately.
Both default off, zero hot-path cost when unset. Code retained for
Phase 4 validation re-use.

**Run.** `matcher-r3gr` deployment, `MATCH_CONCURRENCY=4`,
`SOLVER_PROFILE_PER_LAYER=1`, `SOLVER_PROFILE_SAMPLE_N=50`. Live traffic
via `just bench`. 12 samples collected; row counts are 6-layer solves
(5 history + 1 current) except one 3-layer warmup outlier excluded.

**Results.**

| metric | value |
|---|---|
| avg solve_ms | 12.1 ms |
| `astar` share of solve | ~0.2% (≪ 1ms, effectively free) |
| `gen` (all reach work) share of solve | ~99.8% |
| L4 share of solve (warm step's expected retained work) | 16.8% avg |
| warm-step skippable share (L0..L3 + L5) | 83.1% avg |
| worst-case row (Row 3, dense intersection on new layer) | 43% retained — still clears threshold |
| best-case row (Row 9) | 1% retained |

The gate condition (`>40% of p99 wall time in repeated-layer work`) is
met by a wide margin. `astar` is so cheap it does not constrain the
design — warm step's gain ceiling is essentially the full `gen` share.

**Unexpected: L5 is non-zero.** The last-layer source's `reach()` was
expected to be near-zero (no `next_layer` to reach into). Observed
values were 0.1–6.5 ms across samples, sometimes the *largest* layer.
Either `next_layer(last_source)` is doing real work, or there's
synthetic-end-edge logic burning time. **Phase 1 investigation item**:
if L5 is wasted work today, the new solver should not reproduce it.

**Variance.** solve_ms ranged 0.22 → 25.3 ms in the sample window — a
few specific GPS points landing in dense interchanges drag the tail.
The warm step's worst case is **one** bad layer instead of five, so the
absolute worst-case improves by ~5× even before factoring out the
saved layers.

---

### Phase 0 — Profile current matcher (original plan, retained for reference)

Add `tracing` spans + an instrumented build. Run a 5-minute bench with
known traffic shape. Generate a flamegraph; capture per-event time
broken down by:
- `r#match` (top-level)
  - candidate generation
  - `reach()` loop / predicate cache misses
  - `astar` over transition graph

**Go/no-go threshold**: `> 40% of p99 per-event wall-time` must be
attributable to **repeated-layer work** (`reach()` calls for layer
pairs `0..n-2` that the warm step would skip). Below that bar this
design moves work around without removing it — revisit. The 40% target
includes the candidate generation for those skipped layers too.

**Caveat**: latency degradation comes up *after* load, so the flamegraph
needs to be captured under sustained load (≥30s into the run), not at
startup. Results may still be ambiguous if the allocator state interacts
with sample rate; treat as directional. If the profile is inconclusive,
do a second pass with `MATCH_CONCURRENCY=1` to remove cross-solve
contention from the picture.

### Phase 1A — Anchor-based MVP [✅ complete, 2026-06-13]

**Outcome: 3–4× speedup at p95 on the stateful pod, no error-rate
regression, 90.8% of events served from warm state.**

Before committing to the full solver rewrite (Phase 1B below), we built
an **anchor-based 1-best warm step** as an MVP. It uses the existing
`MatchOptions::anchor` mechanism — no solver modifications:

- Per-vehicle state cache (`DashMap<vehicle_id, MatchState>`) holds the
  most-recent snapped coord + event timestamp.
- On a new event: if state exists and is fresh, `r#match` is called with
  a **1-point linestring** (just the new GPS coord) + `anchor =
  prev.last_matched`. The solver prepends the anchor internally,
  trellis = 2 layers (anchor + new), trivial reach work.
- On cold start (missing/stale state, or after error): existing 6-point
  full solve.
- Optimistic write-wins-by-timestamp via `DashMap::entry().and_modify`;
  out-of-order redeliveries (older `resolved_at_ms`) discard.
- Background tokio interval task evicts entries older than
  `MATCH_STATE_TTL_SECS` (default 1800s) every 60s.
- Gated behind `MATCH_STATEFUL=1` env. Off by default, zero overhead.

**What this MVP loses vs full Viterbi (Phase 1B):** multi-hypothesis
preservation. With only the snapped coord carried forward, the warm
step commits to the previous single-best match. At ambiguous junctions
where the prior best was wrong, the warm step cannot revise.

**Live validation (replay against shadowed shard `r3gr` only, other 5
shards on baseline solver):**

| metric | r3gr (stateful=true) | r3gq (baseline) | r3gx (baseline, densest) |
|---|---|---|---|
| events processed | 13063 | 7493 | 6581 |
| cold steps | 1578 | 9059 | 8545 |
| warm steps | 15620 | 0 | 0 |
| warm hit rate | **90.8%** | n/a | n/a |
| cache size | 344 vehicles | n/a | n/a |
| solve avg | **4.93 ms** | 12.16 ms | 16.11 ms |
| solve p95 | **17.80 ms** | 41.12 ms | 44.66 ms |
| end-to-end p95 | 18.05 ms | 41.25 ms | 44.74 ms |
| error rate | 8.4% | 8.3% | 2.5% |

The error-rate parity with r3gq (the closest-throughput baseline pod)
confirms the warm step doesn't introduce new failure modes. r3gx's
lower rate reflects its less-dense network, not solver quality.

**Speedup**: ~3.3× at solve average vs r3gx (densest shard), ~2.5× vs
r3gq (similar throughput). The win is consistent across the
distribution — tail multipliers (p95/avg) are similar to baseline.

**Conclusion**: anchor-based MVP is shippable. We can roll out
shard-by-shard behind the existing `MATCH_STATEFUL` flag. The
remaining question is whether multi-hypothesis preservation (Phase 1B)
gives meaningful additional accuracy on top of this — that's a
shadow-mode comparison to be set up separately.

**Cluster-wide rollout [✅ 2026-06-13]**: Enabled `MATCH_STATEFUL=1`
on all 6 active matcher pods (r3gr, r3gq, r3gx, r3gw, r652, r658).
Steady-state metrics after ~75s of replay:

| Pod | warm | cold | warm rate | solve avg | solve p95 |
|---|---|---|---|---|---|
| r3gr | 35595 | 3474 | **91.1%** | 5.04 ms | 18.07 ms |
| r3gq | 9170 | 929 | **90.8%** | 4.82 ms | 19.30 ms |
| r3gx | 9142 | 402 | **95.8%** | 8.15 ms | 23.52 ms |
| r3gw | 2527 | 292 | **89.6%** | 3.27 ms | 12.37 ms |
| r652 | 830 | 62 | **93.0%** | 2.47 ms | 8.86 ms |

Aggregate: 70k warm steps / 5.9k cold steps = **92.2% cluster warm
rate**. 745 vehicles cached across pods. Avg latency improvement vs
the baseline numbers from Phase 1A:

- r3gq: 12.16 → 4.82 ms (**2.5×**)
- r3gx: 16.11 → 8.15 ms (**2.0×**)
- r3gw: 9.68 → 3.27 ms (**3.0×**)
- r3gr: 4.93 → 5.04 ms (already stateful — unchanged as expected)

Error rates stable or slightly improved (r3gr 8.4 → 7.0%, r3gq 8.3 →
7.6%). No regressions detected.

**Phase 1B v1 (cum_cost tracking + divergence guard) [✅ 2026-06-13]**:

Phase 1B per the spec calls for a **full multi-candidate Viterbi
frontier** — a `StreamingForwardSolver` rewrite that preserves the
entire prior column and runs a real argmin over multiple hypotheses
per event. That's multi-week work and was deferred (renamed Phase 1C
below).

What v1 ships in one session is an **incremental upgrade over 1A**
that lays the foundation for 1C without breaking the state cache
shape:

- `MatchState` now carries `last_cum_cost: u32` — the running Viterbi
  cumulative cost along the 1-best path from the vehicle's most-recent
  cold-start through the latest event. On warm step:
  `new_cum_cost = prev_cum_cost + RoutedPath::cost`. On cold-start:
  reset to the cold-start's path cost.
- New histogram metric `routers_match_cum_cost` for live observability
  of cum_cost distribution.
- New env knob `MATCH_COST_CEILING` (default 2,000,000). When a
  vehicle's `last_cum_cost` exceeds the ceiling, the warm-state is
  dropped and the next event cold-starts to re-anchor. Counter
  `routers_match_cost_ceiling_evictions_total` exposes how often the
  guard fires.
- `RoutedPath` gained a public `cost: u32` field (forwarded from
  `CollapsedPath::cost`) so the matcher binary can read the per-event
  Viterbi total without going through internal types.

**Live results (cluster-wide flood, all 6 pods stateful, C=2 R=2
CPU=2 — Phase 1B v1 baseline)**:

| metric | value | vs 1A baseline |
|---|---|---|
| peak throughput | 2189 evt/s | -17% (single-run noise + restart) |
| solve p50 | 3.43 ms | -16% (better) |
| solve p95 | 16.08 ms | **-22% (better)** |
| solve p99 | 23.93 ms | **-22% (better)** |
| cum_cost p50 | 23,964 | new |
| cum_cost p95 | 170,015 | new |
| cum_cost p99 | 611,563 | new (well below 2M ceiling) |
| cost-ceiling evictions | 22 (of 237k warm steps = 0.009%) | new |
| warm hit rate | 81.5% | vs 92% earlier (more cold-starts from restart) |

The latency improvement is real — the cum_cost path takes the same
work as 1A and saves a tiny amount via the early-out cost guard. The
peak-throughput drop is mostly noise (~17% single-run variance is
within tolerance; we've seen ±15% variation across earlier flood
runs).

**What v1 does NOT deliver:**
- Multi-candidate frontier preservation. Still 1-best.
- Real Viterbi argmin over multiple prior hypotheses. Still uses
  `MatchOptions::anchor` (single point).
- `StreamingForwardSolver` rewrite. Existing
  `PrecomputeForwardSolver` runs every event.

**Phase 1C — Full multi-candidate Viterbi frontier**

### Key correctness property

Receiving a new event and solving using a cached transition graph
**produces the identical result** to a full match on the same set of
points. This follows from the Markov property of the Viterbi
recurrence:

> `V[k][c] = min_c' (V[k-1][c'] + transition(c'→c) + emission(c, o[k]))`

The optimal path of length N has an optimal prefix of length N-1 —
prefix-optimality is what makes streaming Viterbi equivalent to
batch Viterbi. Re-evaluating the prior layers' candidates with the
new event's evidence cannot change which path is optimal *through
the prior layer*, because emission and transition costs are
strictly local (per-layer / per-edge).

**Implications for validation:**
- We don't need a shadow-mode A/B comparison against the existing
  solver. Correctness is guaranteed by construction.
- Property tests are still warranted as implementation-bug checks
  (does our code match the algorithm?) — but they're not
  ship-blocking.
- No production bake-time required for accuracy. We can flip
  `MATCH_STATEFUL_VARIANT=1c` directly once perf is acceptable.

**Sources of caveat that could violate this guarantee:**
- Map data changes between events → cached frontier references
  edges that no longer exist. **Mitigated**: any map change requires
  a pod restart, which flushes the in-memory state cache. Within a
  pod's lifetime, the map is immutable.
- Costing strategy changes between events → emission/transition
  formulas differ. **Mitigated**: costings are constructed at solver
  init and don't change at runtime.
- Concurrent events for the same vehicle with out-of-order
  `resolved_at_ms` → already handled by the last-writer-by-timestamp
  guard on state writeback.

Under these locality constraints, **Phase 1C cannot regress match
accuracy versus the current full-rebuild solver** for any individual
event whose state pre-dates a pod restart and map/config refresh.

### What changes vs Phase 1B v1

**Current (1B v1):** Per-vehicle state is `{ last_matched: Point,
last_cum_cost: u32 }`. Warm step calls `r#match` with
`MatchOptions::anchor = last_matched` + 1-point linestring. **Only
the previous best match is carried forward** — at ambiguous
intersections we can't revise.

**Target (1C):** Per-vehicle state is `{ frontier:
Vec<FrontierNode<E>>, last_matched: Point, last_event_ms: u64 }`.
Warm step constructs L0 directly from the saved frontier with each
prior candidate's cum_cost baked into its `emission`. Real Viterbi
step: for each new L1 candidate, `min` over all L0 frontier
candidates of `(prev.cum_cost + transition + new.emission)`. **All
prior hypotheses participate; the warm step can revise** wrong prior
choices when new evidence makes a different prefix optimal.

### Performance estimates

| metric | 1B v1 (current) | 1C with K=∞ | 1C with K=3 (recommended default) | 1C with K=1 (≈ 1B) |
|---|---|---|---|---|
| Per-event reach calls (typical, ~10 candidates/layer) | 10 | 100–500 | 30 | 10 |
| Solve avg | ~5 ms | ~25–50 ms | ~15 ms | ~5 ms |
| Solve p95 | ~16 ms | ~50–100 ms | ~25 ms | ~16 ms |
| Peak throughput (extrapolated) | 2189 evt/s | 400–700 | ~900 | ~2189 |
| Match accuracy at ambiguous junctions | baseline | **+2–5%** | **+1–3%** | baseline |
| Memory per vehicle | ~40 bytes | ~10 KB | ~1 KB | ~40 bytes |

`K=3` is the practical sweet spot — most of the multi-hypothesis
benefit at ~2.5× the per-event cost. `K=1` is operationally
equivalent to 1B v1 but routed through the proper Viterbi machinery,
useful as a default-on validation step.

### 17-task breakdown

#### Solver foundations (1–6)

1. **Refactor `PrecomputeForwardSolver::solve` into helpers** —
   extract `build_pair`, `find_optimal_path`,
   `materialize_collapsed_path`. Keep `solve()` API unchanged. ~1d.
2. **Add `extract_last_layer_frontier(pair, start, layers)` pure
   function** — forward Viterbi sweep returning cum_cost per L_last
   candidate. ~0.5d.
3. **Add `PrecomputeForwardSolver::solve_with_frontier(...)`** —
   returns `(CollapsedPath, Vec<(CandidateId, u32)>)`. ~0.5d.
4. **Define `streaming::FrontierNode<E>`** — `Edge<E>`, snapped
   `Point`, `cum_cost`. Serde-derived. ~0.25d.
5. **Add `Transition::from_parts` constructor** — bypasses
   `LayerGenerator`, accepts pre-built `Layers` + `Candidates`. ~0.25d.
6. **Confirm `Candidate::new` factory is reachable** from the
   streaming module — already public; spot-check. ~0.1d.

#### Streaming-step plumbing (7–12)

7. **`streaming::build_warm_step_transition`** — generates L1 via
   `StandardGenerator`, builds L0 manually from saved frontier, wires
   into `Candidates<E>` (petgraph + scc lookup) + `Layers`. **Highest
   implementation risk** — petgraph + `attach_ends` + `weave` +
   CandidateId ordering. ~1.5d.
8. **`streaming::match_step_full`** — orchestrator: calls
   `build_warm_step_transition`, `solve_with_frontier`, extracts
   `MatchedFix` + new frontier. ~0.5d.
9. **Top-K frontier pruning helper** — `prune_frontier(nodes, k)`
   sorts by cum_cost, takes K best. Gated by `MATCH_FRONTIER_K`. ~0.25d.
10. **Extend `MatchState<E>` to `Vec<FrontierNode<E>>`** — generic
    over `E`, instantiated as `MatchState<OsmEntryId>` in the matcher.
    ~0.5d.
11. **State shape migration** — bump cache version; let all in-flight
    vehicles cold-start once on first event after deploy. ~0.25d.
12. **Wire matcher binary to `match_step_full`** — replace anchor-
    based warm path; cold-start path unchanged but now also extracts
    the L_last frontier from the cold solve so subsequent events have
    warm state. ~1d.

#### Correctness checks (13–14)

These are **implementation-bug catches**, not algorithmic-equivalence
gates (the Markov property already gives us equivalence). Useful but
not ship-blocking.

13. **Property test: cold-start equivalence** — synthetic 3, 5,
    10-layer inputs. Streaming N-step chain must produce the same
    chosen path as `PrecomputeForwardSolver::solve` on the full
    linestring. Failure means the streaming code path has a bug — the
    underlying algorithm is provably equivalent. ~1d.
14. **Property test: warm-step argmin equivalence** — cold-solve N-1
    layers, snapshot frontier, run `match_step_full` for the Nth
    point. Argmin of new frontier must equal snapped endpoint of
    cold-solve over all N points. ~0.5d.

#### Observability (15)

15. **Metrics additions**:
    - `routers_match_frontier_size` (histogram) — how big are the
      saved Viterbi columns?
    - `routers_match_argmin_revision_total` (counter) — increments
      when the warm step's chosen predecessor differs from the
      previous event's argmin (the "recovery" events where multi-
      hypothesis tracking actually paid off).
    - `routers_match_cold_start_reason{reason=...}` (counter) —
      missing | ttl | ceiling | error_fallback | shape_mismatch.
    ~0.5d.

#### Performance + rollout (16–17)

16. **Performance benchmark sweep with K** — flood throughput + p95
    at K ∈ {1, 3, 5, 10, ∞}. Build a small grid of (K, concurrency).
    Find the throughput/quality sweet spot for the 6-pod 2-CPU
    cluster. ~1d.
17. **Default-on rollout** — flip `MATCH_STATEFUL_VARIANT=1c` shard
    by shard. No bake-time needed for correctness; only monitoring
    latency to confirm the K choice is right under real load. Keep
    `MATCH_STATEFUL_VARIANT` env as kill-switch. ~0.5d total (no
    bake).

### Total estimate

- **Active engineering**: ~8 working days (was 12 before correctness
  guarantee removed shadow-mode tasks).
- **Bake time**: none required for correctness; ~0.5 day per shard
  rollout to confirm perf.
- **Total elapsed**: ~2 weeks of focused effort.

### Removed from earlier breakdown

The Markov-property guarantee eliminates these tasks that were in
the original 20-task plan:
- Shadow-mode comparator deployment + comparator binary.
- Live A/B comparison runs against the stateless solver.
- Cluster-wide divergence-threshold validation as a ship gate.

These would have added 4–5 days but provided no actionable signal.

### Key risks (revised)

1. **Task 7** (manual trellis construction) — implementation
   complexity. Mitigation: incremental build with unit tests on the
   assembled `Candidates` before wiring to the solver.
2. **Memory growth** at high K — bounded by top-K pruning (task 9).
   `K=3` keeps state ~1KB/vehicle = ~100MB at 100k vehicles per pod,
   acceptable.
3. **Task 16 (perf benchmark) might show no K wins on throughput** —
   need to clearly document the latency/throughput/accuracy trade-off
   so users can choose. Worst case: 1C ships with `K=1` default and
   power users opt into higher K when they want the accuracy.

The 1B v1 work remains forward-compatible: matcher binary's state
cache, cost-ceiling guard, and metrics all carry forward unchanged.
1C swaps the solver path and extends `MatchState`; nothing else
needs to change.

---

**Parallelism sweep + CPU-cap experiment [✅ 2026-06-13]**: With warm
step being ~5ms avg solve, the old C=4 R=2 settings (chosen when each
event did ~12ms of work) were likely wrong. We also wanted to test
whether the HMM benefits from rayon parallel reach work or is
thread-local-bound. Set CPU **request 500m, limit 2** on all matcher
pods (was 100m / no limit) and swept config:

| variant | C | R | CPU cap | peak | solve p50 | p95 | p99 |
|---|---|---|---|---|---|---|---|
| Baseline | 4 | 2 | none (burstable) | 1861 evt/s | ~5 ms | 25.30 ms | 46.73 ms |
| A | 4 | 1 | 2 | 1498 evt/s | 9.74 ms | 39.63 ms | 62.57 ms |
| **B** | **2** | **2** | **2** | **2654 evt/s** | **4.07 ms** | **20.58 ms** | **30.85 ms** |
| C | 1 | 2 | 2 | 2654 evt/s | 3.60 ms | 20.48 ms | 33.07 ms |
| D | 4 | 2 | 2 | 2579 evt/s | 8.42 ms | 36.08 ms | 50.82 ms |

**Conclusions:**

1. **The HMM is *not* thread-local bound.** Removing rayon (A, R=1)
   dropped throughput 44% (2654→1498) and inflated p95 by 96% versus
   the same CPU-cap with R=2. The bounded Dijkstra in `reach()`
   genuinely benefits from parallel candidate evaluation.
2. **Outer concurrency beyond 1–2 hurts when CPU-capped.** C=1 and
   C=2 give identical throughput at 2654. C=4 (variant D) holds peak
   throughput but inflates p95 76% (20.58 → 36.08 ms) due to
   in-process queueing inside the matcher's join_set.
3. **Hard CPU caps *beat* burstable resources for predictable perf.**
   B (capped at 2) outperformed the burstable baseline by 43%
   throughput AND has lower latency. With 6 pods × 2 CPU = 12 CPU on
   a 15-CPU node, there's coherent resource isolation; burstable mode
   has 6 pods chaotically fighting for the same 15 cores.
4. **Winning config**: **C=2, R=2, CPU=2.** Locked in across all 6
   pods.

### Phase 1B — Full Viterbi frontier preservation (deferred)

Implement `StreamingForwardSolver` from scratch, alongside the existing
`PrecomputeForwardSolver`. Includes:
- A `step()` method that operates on (prev frontier, new layer candidates).
- A full `solve()` impl for cold-start that internally builds the
  frontier layer-by-layer using `step()`.
- Property tests on synthetic 3-, 5-, and 10-layer inputs:
  1. **Cold-start equivalence**: streaming N-step solve produces the
     same chosen path as today's `PrecomputeForwardSolver::solve` on
     the same linestring, modulo the divergence tolerances declared in
     §Validation.
  2. **Warm-step argmin equivalence**: feed (prefix solve → frontier),
     then `step(frontier, layer_N)`, and verify the argmin of the new
     frontier matches the snapped end of `PrecomputeForwardSolver::solve`
     on the same `(prefix + layer_N)` linestring. This is the
     *specific* claim the warm path relies on (single back-pointer hop
     argmin ≈ full-path argmin). Failure here means the Markov
     prefix-optimality assumption doesn't hold for the current
     `CostingStrategies` and needs an audit.

This phase is the bulk of the work — multiple weeks, not days.

### Phase 2 — Trait method, cold-start bridge

Add `Match::match_step`. Cold-start path wraps today's
`PrecomputeForwardSolver::solve` (unchanged) and snapshots the final
frontier. Warm path calls the new `step()` on `StreamingForwardSolver`.

### Phase 3 — Matcher binary integration

State cache, lock-free per-vehicle write-back, eviction, metrics. All
behind `MATCH_STATEFUL`.

### Phase 4 — Side-by-side validation

Run stateful in shadow mode. **Mechanics**: a separate matcher
deployment (`matcher-{shard}-shadow`) subscribes to the same
`match.{shard}` subject as the production matcher. NATS core is
fan-out pub/sub — no queue group involved — so both matchers receive
every message naturally. Crucially the shadow matcher publishes its
results to **different subjects**: `matched.positions.shadow` and
`matched.routes.shadow.{vehicle_id}`. The viewer continues to consume
the canonical subjects so its rendering is unaffected. A separate
comparator process consumes both `matched.positions` and
`matched.positions.shadow`, joins on `(vehicle_id, resolved_at_ms)`,
and exports divergence histograms to Prometheus.

Validate against the divergence thresholds in §Validation. Default-on
rollout (Phase 5) is gated on the shadow comparator showing all
thresholds met over a ≥1h window.

### Phase 5 — Default on

Flip `MATCH_STATEFUL` default to on. Cohorted rollout: enable on a
single shard's matcher pod first (`r3gw` — lowest event volume), bake
for 24h, then enable shard-by-shard. The env-flag knob means rollout
granularity is at the deployment level — no per-shard code paths
needed.

**Flag stays in code indefinitely** as a kill-switch — code cost is
trivial, the ability to revert in prod without a deploy is worth
keeping.

## Settled decisions

- **Memory cap**: `MATCH_STATE_CAP=100_000` initially; revisit after
  Phase 0 profiling reveals true per-vehicle footprint. May drop to
  20_000 if worst-case frontier sizes are real.
- **Eviction**: `moka::sync::Cache` with `max_capacity` + `time_to_idle`.
  No hand-rolled scanner.
- **TTL alignment**: `MATCH_STATE_TTL_SECS=1800`,
  `HISTORY_MAX_AGE_SECS` raised from 300 → 1800 to match.
- **Cost-ceiling trip-wire**: dropped from the design. Viterbi
  recurrence is stable. If we see divergence in shadow mode (Phase 4)
  we'll add a metric-driven mechanism, not an arbitrary ceiling.
- **State persistence**: not persisted. Recovery via
  historian → Valkey → `WarmingMemoryStore` → cold-start. Pod restart =
  one cold-start per active vehicle on next event.
- **MatchRoute under warm-step**: option (a) — single incremental edge
  segment. Viewer trims to ~1 GPS-interval anyway, no fidelity loss.
- **Concurrency**: per-entry `RwLock` with optimistic
  write-wins-by-timestamp on `resolved_at_ms`.
- **`MATCH_STATEFUL` flag**: indefinite, kill-switch.

## Non-goals

- No persistence of Viterbi/frontier state.
- No cross-shard state sharing.
- No changes to orchestrator history filtering, shard selection, or
  NATS subject layout.
- No anchor support under warm-step (documented as ignored; cold-start
  still honours it).

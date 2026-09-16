//! Resolves a vehicle to its serving region. (T07)
//!
//! Every observation must be pinned to exactly one solve region before a job
//! can be addressed to it. Which region that is depends only on the
//! observation's geohash cell, the mounted [`Catalog`], and what the vehicle's
//! checkpoint remembers about where it was last solved (its [`Pin`]). It never
//! depends on live infrastructure: the resolver reads the catalog that was
//! mounted at start-up and nothing else, so routing a single event never costs
//! a Kubernetes lookup and every replica resolves a given input identically.
//!
//! # Why hysteresis
//!
//! A vehicle sitting on a regional border would otherwise flap between two
//! regions as consecutive fixes land on either side of the line, tearing its
//! continuity apart and thrashing the two regions' matcher caches. Each region
//! therefore certifies a ring of `overlap` cells it can also serve. While a
//! pinned region still covers the current cell through its coverage *or* its
//! overlap padding — and still runs the same graph the vehicle was solved
//! against — the resolver keeps the vehicle there ([`ResolutionKind::Kept`]).
//! Only when the vehicle leaves that padded region, or the region's graph is
//! upgraded underneath it, does it repin.

use core::time::Duration;

use geo::Point;
use thiserror::Error;

use crate::event::{VehicleId, shard_of};
use crate::partition::mix;
use crate::protocol::ids::{GraphVersion, Lane, RegionId};
use crate::region::catalog::{Catalog, Region};

/// What a vehicle's checkpoint remembers about where it was last solved.
///
/// The dispatcher reconstructs a `Pin` from the fields a
/// [`VehicleCheckpoint`](crate::store::checkpoint::VehicleCheckpoint) already carries (`region` + `graph`)
/// plus the `routing_version` that catalog was on, and hands it to
/// [`Resolver::resolve`] so a border-straddling vehicle can be kept on its
/// current region (see the module docs on hysteresis).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    /// The region the vehicle was last solved in.
    pub region: RegionId,
    /// The graph snapshot that solve ran against. Compared against the
    /// catalog's current graph for the region: a mismatch breaks hysteresis so
    /// the vehicle is re-resolved (and its continuity restarted downstream)
    /// rather than kept on a stale network.
    pub graph: GraphVersion,
    /// The catalog `routing_version` in effect when the vehicle was pinned.
    /// Only [`Resolver::routing_changed`] consults it; `resolve` deliberately
    /// ignores it so the two concerns stay separable at the callsite.
    pub routing_version: u64,
}

/// How a [`Resolution`] was reached. A bounded set, safe to use as a metric
/// label via [`label`](Self::label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionKind {
    /// The pinned region still serves this cell on the same graph, so the
    /// vehicle stayed put (boundary hysteresis).
    Kept,
    /// The vehicle moved into a cell a different region owns, so it was repinned
    /// to that owner.
    Repinned,
    /// No region owns the cell, but one certifies it as an `overlap` fallback,
    /// so the vehicle was placed there.
    Fallback,
}

impl ResolutionKind {
    /// A stable, low-cardinality label for metrics and logs.
    pub fn label(self) -> &'static str {
        match self {
            ResolutionKind::Kept => "kept",
            ResolutionKind::Repinned => "repinned",
            ResolutionKind::Fallback => "fallback",
        }
    }
}

/// The region a vehicle is pinned to for one observation, with everything the
/// dispatcher needs to address and deadline the job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The serving region's id (a subject/stream token).
    pub region: RegionId,
    /// The graph snapshot that region's matchers run.
    pub graph: GraphVersion,
    /// The job lane this vehicle takes within the region (see
    /// [`Resolver::resolve`] for how it is derived).
    pub lane: Lane,
    /// The region's per-job freshness budget, from which the dispatcher derives
    /// the job's absolute `deadline_us`.
    pub budget: Duration,
    /// How this resolution was reached.
    pub kind: ResolutionKind,
    /// The catalog `routing_version` this resolution was computed against, so a
    /// downstream consumer can refuse to mix results from different routings.
    pub routing_version: u64,
}

/// No region — owner or fallback — serves the observation's cell. The
/// dispatcher turns this into a `Terminal { UnsupportedCoverage }` outcome for
/// the vehicle rather than dropping the event silently.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("no region serves cell {cell}")]
pub struct Unserved {
    /// The unserved cell, as its canonical base-32 string.
    pub cell: String,
}

/// Resolves observations to serving regions against a borrowed [`Catalog`].
///
/// Holds only a shared reference: the catalog is mounted read-only and shared
/// across every partition worker, and resolution is a pure function of the
/// catalog plus the call's arguments, so a `Resolver` is cheap to construct per
/// call and needs no state of its own.
pub struct Resolver<'c> {
    catalog: &'c Catalog,
}

impl<'c> Resolver<'c> {
    /// Borrow a catalog to resolve against.
    pub fn new(catalog: &'c Catalog) -> Self {
        Self { catalog }
    }

    /// Resolve `vehicle` at `point`, honouring its `pinned` region if one still
    /// applies.
    ///
    /// The cell is `event::shard_of(point)`; the region is chosen by, in order:
    ///
    /// 1. **Keep** the pinned region if it still exists, runs the same graph the
    ///    vehicle was pinned on, and covers the cell through its coverage *or*
    ///    its overlap padding (border hysteresis).
    /// 2. otherwise **repin** to the region that owns the cell;
    /// 3. otherwise **fall back** to the certified `overlap` region with the
    ///    smallest id (a deterministic tiebreak, so every replica agrees);
    /// 4. otherwise the cell is [`Unserved`].
    pub fn resolve(
        &self,
        vehicle: VehicleId,
        point: Point,
        pinned: Option<&Pin>,
    ) -> Result<Resolution, Unserved> {
        let cell = shard_of(point);

        // 1. Hysteresis: stay on the pinned region while its padded coverage
        //    still holds the cell and its graph is unchanged. Routing version
        //    is intentionally *not* consulted here — that is `routing_changed`'s
        //    job, kept a separate query so the dispatcher can decide when a
        //    routing bump should force a re-resolve.
        if let Some(pin) = pinned
            && let Some(region) = self.catalog.region(&pin.region)
            && region.graph == pin.graph
            && (region.coverage.contains(&cell) || region.overlap.contains(&cell))
        {
            return Ok(self.resolution(vehicle, region, ResolutionKind::Kept));
        }

        // 2. The region that owns the cell.
        if let Some(region) = self.catalog.owner_of(&cell) {
            return Ok(self.resolution(vehicle, region, ResolutionKind::Repinned));
        }

        // 3. The lowest-id region certifying the cell as an overlap fallback.
        //    `fallbacks_for` yields candidates in catalog order; picking the
        //    smallest id makes the choice independent of document ordering.
        if let Some(region) = self
            .catalog
            .fallbacks_for(&cell)
            .min_by(|a, b| a.id.cmp(&b.id))
        {
            return Ok(self.resolution(vehicle, region, ResolutionKind::Fallback));
        }

        // 4. Nothing serves it.
        Err(Unserved {
            cell: cell.to_string(),
        })
    }

    /// Whether the vehicle's pin was made against a stale routing.
    ///
    /// Exposed as its own query rather than folded into [`resolve`](Self::resolve)'s
    /// result: a routing bump must force a re-resolve even for a vehicle that
    /// `resolve` would have `Kept`, and the dispatcher decides that at the
    /// callsite. Keeping it separate also stops `resolve` from having to smuggle
    /// an auxiliary flag alongside the region it chose.
    pub fn routing_changed(&self, pinned: &Pin) -> bool {
        pinned.routing_version != self.catalog.routing_version
    }

    /// Assemble a [`Resolution`] for a chosen region.
    fn resolution(&self, vehicle: VehicleId, region: &Region, kind: ResolutionKind) -> Resolution {
        Resolution {
            region: region.id.clone(),
            graph: region.graph.clone(),
            lane: Self::lane_for(vehicle, region),
            budget: region.freshness_budget,
            kind,
            routing_version: self.catalog.routing_version,
        }
    }

    /// The job lane a vehicle takes within a region.
    ///
    /// `mix` is the same splitmix64 avalanche the partitioner uses, so a
    /// vehicle's id spreads evenly over the region's `lanes` regardless of how
    /// the upstream ids are structured, and the mapping is a pure function of
    /// the id — the same vehicle always lands on the same lane (stable), while
    /// the fleet as a whole is balanced across lanes (spread).
    fn lane_for(vehicle: VehicleId, region: &Region) -> Lane {
        Lane((mix(vehicle.0) % u64::from(region.lanes)) as u8)
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;

    use super::*;

    // Four cities, far enough apart that their precision-4 geohash cells are
    // guaranteed distinct (a cell is ~39 km wide). The catalog is built from
    // whatever cells these actually hash to, so the tests never hard-code a
    // geohash string.
    fn sydney() -> Point {
        Point::new(151.2093, -33.8688)
    }
    fn melbourne() -> Point {
        Point::new(144.9631, -37.8136)
    }
    fn brisbane() -> Point {
        Point::new(153.0251, -27.4698)
    }
    fn perth() -> Point {
        Point::new(115.8605, -31.9505)
    }

    fn cell(point: Point) -> String {
        shard_of(point).to_string()
    }

    fn rid(s: &str) -> RegionId {
        RegionId::new(s).unwrap()
    }

    fn gv(s: &str) -> GraphVersion {
        GraphVersion::new(s).unwrap()
    }

    /// A two-region catalog:
    ///
    /// * `west` owns Sydney's cell and certifies Melbourne's and Brisbane's as
    ///   overlap fallbacks; four lanes.
    /// * `east` owns Melbourne's cell and certifies Brisbane's as an overlap;
    ///   one lane.
    ///
    /// So Melbourne's cell exercises hysteresis (owned by `east`, padded by
    /// `west`), Brisbane's exercises fallback (owned by nobody, offered by
    /// both), and Perth's is unserved. `west` is listed first but sorts after
    /// `east`, so a fallback tie proves the id-order tiebreak, not document
    /// order.
    fn build_catalog(routing_version: u64) -> Catalog {
        let toml = format!(
            r#"
version = 1
routing_version = {rv}

[[regions]]
id = "west"
graph = "g"
coverage = ["{syd}"]
overlap = ["{mel}", "{bris}"]
lanes = 4
resource_class = "c"
replicas = {{ min = 1, max = 1 }}
freshness_budget_ms = 30000

[[regions]]
id = "east"
graph = "g"
coverage = ["{mel}"]
overlap = ["{bris}"]
lanes = 1
resource_class = "c"
replicas = {{ min = 1, max = 1 }}
freshness_budget_ms = 45000
"#,
            rv = routing_version,
            syd = cell(sydney()),
            mel = cell(melbourne()),
            bris = cell(brisbane()),
        );
        Catalog::parse(&toml).expect("test catalog should parse")
    }

    #[test]
    fn fixture_cities_map_to_distinct_cells() {
        // The whole fixture rests on these four cells being different; assert it
        // rather than assume it.
        let cells: BTreeSet<String> = [sydney(), melbourne(), brisbane(), perth()]
            .into_iter()
            .map(cell)
            .collect();
        assert_eq!(cells.len(), 4, "fixture cities collided in one cell");
    }

    #[test]
    fn owner_hit_repins() {
        let catalog = build_catalog(7);
        let resolver = Resolver::new(&catalog);

        let res = resolver.resolve(VehicleId(1), sydney(), None).unwrap();

        assert_eq!(res.region, rid("west"));
        assert_eq!(res.graph, gv("g"));
        assert_eq!(res.kind, ResolutionKind::Repinned);
        assert_eq!(res.budget, Duration::from_millis(30_000));
        assert_eq!(res.routing_version, 7);
    }

    #[test]
    fn hysteresis_keeps_pinned_region_on_overlap_cell() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

        // Melbourne's cell is owned by `east` but padded by `west`. A vehicle
        // pinned to `west` on the current graph is kept there.
        let pin = Pin {
            region: rid("west"),
            graph: gv("g"),
            routing_version: 1,
        };
        let res = resolver
            .resolve(VehicleId(2), melbourne(), Some(&pin))
            .unwrap();

        assert_eq!(res.region, rid("west"));
        assert_eq!(res.kind, ResolutionKind::Kept);
        // Kept means it inherits west's budget, not the owning east's.
        assert_eq!(res.budget, Duration::from_millis(30_000));
    }

    #[test]
    fn graph_upgrade_breaks_hysteresis() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

        // Same cell, but the pin remembers an older graph than `west` now runs,
        // so hysteresis cannot apply and the cell's owner (`east`) takes it.
        let pin = Pin {
            region: rid("west"),
            graph: gv("gold"),
            routing_version: 1,
        };
        let res = resolver
            .resolve(VehicleId(3), melbourne(), Some(&pin))
            .unwrap();

        assert_eq!(res.region, rid("east"));
        assert_eq!(res.kind, ResolutionKind::Repinned);
        assert_eq!(res.budget, Duration::from_millis(45_000));
    }

    #[test]
    fn unknown_pinned_region_falls_through_to_owner() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

        // The pin names a region the catalog no longer has; resolution ignores
        // it and repins by ownership.
        let pin = Pin {
            region: rid("ghost"),
            graph: gv("g"),
            routing_version: 1,
        };
        let res = resolver
            .resolve(VehicleId(4), sydney(), Some(&pin))
            .unwrap();

        assert_eq!(res.region, rid("west"));
        assert_eq!(res.kind, ResolutionKind::Repinned);
    }

    #[test]
    fn fallback_picks_lowest_id_not_document_order() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

        // Brisbane's cell is owned by nobody and offered by both regions. `west`
        // is listed first, but `east` sorts lower, so the deterministic pick is
        // `east`.
        let res = resolver.resolve(VehicleId(5), brisbane(), None).unwrap();

        assert_eq!(res.region, rid("east"));
        assert_eq!(res.kind, ResolutionKind::Fallback);
    }

    #[test]
    fn unserved_cell_errors_with_its_cell() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

        let err = resolver.resolve(VehicleId(6), perth(), None).unwrap_err();

        assert_eq!(err.cell, cell(perth()));
    }

    #[test]
    fn lane_is_within_range_stable_and_spread() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

        // `west` (Sydney's owner) has four lanes.
        let mut seen = BTreeSet::new();
        for id in 0..1_000u64 {
            let res = resolver.resolve(VehicleId(id), sydney(), None).unwrap();
            assert!(res.lane.0 < 4, "lane {} outside 0..4", res.lane.0);
            seen.insert(res.lane.0);

            // Resolving the same vehicle again yields the same lane.
            let again = resolver.resolve(VehicleId(id), sydney(), None).unwrap();
            assert_eq!(again.lane, res.lane);
        }
        // The mix genuinely spreads the fleet rather than parking everyone on
        // one lane.
        assert_eq!(seen, BTreeSet::from([0, 1, 2, 3]));
    }

    #[test]
    fn single_lane_region_pins_lane_zero() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

        // `east` (Melbourne's owner) has exactly one lane, so every vehicle maps
        // to lane 0 regardless of its id.
        for id in [0u64, 1, 7, 42, u64::MAX] {
            let res = resolver.resolve(VehicleId(id), melbourne(), None).unwrap();
            assert_eq!(res.lane, Lane(0));
        }
    }

    #[test]
    fn routing_changed_compares_against_catalog() {
        let catalog = build_catalog(5);
        let resolver = Resolver::new(&catalog);

        let current = Pin {
            region: rid("west"),
            graph: gv("g"),
            routing_version: 5,
        };
        assert!(!resolver.routing_changed(&current));

        let stale = Pin {
            routing_version: 4,
            ..current
        };
        assert!(resolver.routing_changed(&stale));
    }

    #[test]
    fn resolution_kind_labels_are_stable() {
        assert_eq!(ResolutionKind::Kept.label(), "kept");
        assert_eq!(ResolutionKind::Repinned.label(), "repinned");
        assert_eq!(ResolutionKind::Fallback.label(), "fallback");
    }
}

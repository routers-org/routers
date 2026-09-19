//! Resolves a vehicle to its serving region.
//!
//! Resolution is a pure function of the observation's geohash cell, the mounted
//! [`Catalog`], and the vehicle's [`Pin`] — never live infrastructure — so every
//! replica resolves a given input identically. A vehicle on a regional border is
//! held on its pinned region while its `overlap` padding still covers the cell
//! (hysteresis), repinning only when it leaves that padding or the graph changes.

use core::time::Duration;

use geo::Point;
use thiserror::Error;

use crate::event::{VehicleId, shard_of};
use crate::partition::mix;
use crate::protocol::ids::{GraphVersion, Lane, RegionId};
use crate::region::catalog::{Catalog, Region};

/// What a vehicle's checkpoint remembers about where it was last solved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    /// The region the vehicle was last solved in.
    pub region: RegionId,
    /// The graph that solve ran against; a mismatch with the catalog breaks hysteresis and re-resolves.
    pub graph: GraphVersion,
    /// The catalog `routing_version` when pinned.
    pub routing_version: u64,
}

/// How a [`Resolution`] was reached; a metric label via [`label`](Self::label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionKind {
    /// The pinned region still serves this cell on the same graph (hysteresis).
    Kept,
    /// The vehicle moved into a cell a different region owns.
    Repinned,
    /// No region owns the cell; one certifies it as an `overlap` fallback.
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
    /// The job lane this vehicle takes within the region.
    pub lane: Lane,
    /// The region's per-job freshness budget, source of the job's `deadline_us`.
    pub budget: Duration,
    /// How this resolution was reached.
    pub kind: ResolutionKind,
    /// The catalog `routing_version` this resolution was computed against.
    pub routing_version: u64,
}

/// No region — owner or fallback — serves the observation's cell.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("no region serves cell {cell}")]
pub struct Unserved {
    /// The unserved cell, as its canonical base-32 string.
    pub cell: String,
}

/// Resolves observations to serving regions against a borrowed [`Catalog`].
pub struct Resolver<'c> {
    catalog: &'c Catalog,
}

impl<'c> Resolver<'c> {
    /// Borrow a catalog to resolve against.
    pub fn new(catalog: &'c Catalog) -> Self {
        Self { catalog }
    }

    /// Resolve `vehicle` at `point`, honouring its `pinned` region if it still applies.
    ///
    /// The region is chosen in order: keep the pinned region while its padded
    /// coverage holds the cell on the same graph; else repin to the cell's
    /// owner; else fall back to the lowest-id `overlap` region; else [`Unserved`].
    pub fn resolve(
        &self,
        vehicle: VehicleId,
        point: Point,
        pinned: Option<&Pin>,
    ) -> Result<Resolution, Unserved> {
        let cell = shard_of(point);

        if let Some(pin) = pinned
            && pin.routing_version == self.catalog.routing_version
            && let Some(region) = self.catalog.region(&pin.region)
            && region.graph == pin.graph
            && (region.coverage.contains(&cell) || region.overlap.contains(&cell))
        {
            return Ok(self.resolution(vehicle, region, ResolutionKind::Kept));
        }

        if let Some(region) = self.catalog.owner_of(&cell) {
            return Ok(self.resolution(vehicle, region, ResolutionKind::Repinned));
        }

        if let Some(region) = self
            .catalog
            .fallbacks_for(&cell)
            .min_by(|a, b| a.id.cmp(&b.id))
        {
            return Ok(self.resolution(vehicle, region, ResolutionKind::Fallback));
        }

        Err(Unserved {
            cell: cell.to_string(),
        })
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

    /// The job lane a vehicle takes within a region (stable per id, spread across lanes).
    fn lane_for(vehicle: VehicleId, region: &Region) -> Lane {
        Lane((mix(vehicle.0) % u64::from(region.lanes.get())) as u8)
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeSet;

    use super::*;

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

    /// `west` owns Sydney, pads Melbourne/Brisbane (4 lanes); `east` owns
    /// Melbourne, pads Brisbane (1 lane). `west` is listed first but sorts after
    /// `east`, so a fallback tie proves the id-order tiebreak, not document order.
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
        assert_eq!(res.budget, Duration::from_millis(30_000));
    }

    #[test]
    fn graph_upgrade_breaks_hysteresis() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

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

        let mut seen = BTreeSet::new();
        for id in 0..1_000u64 {
            let res = resolver.resolve(VehicleId(id), sydney(), None).unwrap();
            assert!(res.lane.0 < 4, "lane {} outside 0..4", res.lane.0);
            seen.insert(res.lane.0);

            let again = resolver.resolve(VehicleId(id), sydney(), None).unwrap();
            assert_eq!(again.lane, res.lane);
        }
        assert_eq!(seen, BTreeSet::from([0, 1, 2, 3]));
    }

    #[test]
    fn single_lane_region_pins_lane_zero() {
        let catalog = build_catalog(1);
        let resolver = Resolver::new(&catalog);

        for id in [0u64, 1, 7, 42, u64::MAX] {
            let res = resolver.resolve(VehicleId(id), melbourne(), None).unwrap();
            assert_eq!(res.lane, Lane(0));
        }
    }

    #[test]
    fn routing_change_breaks_hysteresis_and_repins() {
        let catalog = build_catalog(5);
        let resolver = Resolver::new(&catalog);

        let stale = Pin {
            region: rid("west"),
            graph: gv("g"),
            routing_version: 4,
        };
        let res = resolver
            .resolve(VehicleId(7), melbourne(), Some(&stale))
            .unwrap();
        assert_eq!(res.region, rid("east"));
        assert_eq!(res.kind, ResolutionKind::Repinned);
    }

    #[test]
    fn resolution_kind_labels_are_stable() {
        assert_eq!(ResolutionKind::Kept.label(), "kept");
        assert_eq!(ResolutionKind::Repinned.label(), "repinned");
        assert_eq!(ResolutionKind::Fallback.label(), "fallback");
    }
}

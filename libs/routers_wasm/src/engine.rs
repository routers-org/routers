//! The routing core, free of any wasm/component types so it builds and tests on
//! the host. The engine holds a set of resident **shards** and answers queries
//! over their composite ([`MultiShardNetwork`], a unified `Network`), so a
//! driver can load/unload regions to bound memory and still match across shard
//! boundaries. The component bindings in `bindings.rs` map the WIT interface
//! onto this.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::cmp::Ordering;
use core::str::FromStr;

use geo::{Coord, Distance, Haversine, LineString, Point};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_codec::primitive::context::TripContext;
use routers_codec::primitive::transport::TransportMode;
use routers_network::{Discovery, Edge, Metadata, Node, Route, Scan};
use routers_shard::{
    Geohash, GeohashStrategy, MultiShardNetwork, ShardedNetwork, ShardingStrategy,
};
use routers_transition::candidate::RoutedPath;
use routers_transition::primitives::{MatchError, PredicateCache};
use routers_transition::uom::si::f64::Length;
use routers_transition::uom::si::length::meter;
use routers_transition::weigh::SolverVariant;
use routers_transition::{Match, MatchOptions};

/// Geohash precision for shard ids. The shard producer (`generate-shards`) and
/// the driver computing which shards a viewport needs must agree with this.
pub const SHARD_PRECISION: u8 = 6;

type Shard = ShardedNetwork<OsmEntryId, OsmEdgeMetadata, Geohash>;
type Composite = MultiShardNetwork<OsmEntryId, OsmEdgeMetadata, Geohash>;

/// A routing engine over a dynamic set of loaded shards.
pub struct Engine {
    shards: BTreeMap<String, Arc<Shard>>,
    composite: Composite,
}

/// Resolved match tuning (the WIT `match-options` with defaults applied).
pub struct Options {
    pub search_distance: Option<f64>,
    pub reach_distance: Option<f64>,
    pub solver: SolverVariant,
    /// The vehicle to cost edges for; `None` uses the network's default runtime.
    pub transport: Option<TransportMode>,
}

/// A match failure, with input-point indices where relevant.
#[derive(Debug)]
pub enum Failure {
    Unanchored(Vec<u32>),
    Disconnected(Vec<u32>),
    Solve(String),
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// An engine with no shards loaded. Queries return empty/not-found until a
    /// shard is loaded.
    pub fn new() -> Engine {
        Engine {
            shards: BTreeMap::new(),
            composite: MultiShardNetwork::new(Vec::new()),
        }
    }

    fn rebuild(&mut self) {
        self.composite = MultiShardNetwork::new(self.shards.values().cloned().collect());
    }

    /// Load a shard (`.shard.rt` bytes) under `id`, replacing any existing one,
    /// then recompose. A whole small region is simply one shard.
    pub fn load_shard(&mut self, id: String, bytes: &[u8]) -> Result<(), String> {
        let shard = Shard::from_cached_bytes(bytes)?;
        self.shards.insert(id, Arc::new(shard));
        self.rebuild();
        Ok(())
    }

    /// Evict a shard to free memory; returns whether it was resident.
    pub fn unload_shard(&mut self, id: &str) -> bool {
        let existed = self.shards.remove(id).is_some();
        if existed {
            self.rebuild();
        }
        existed
    }

    /// The ids of the currently-resident shards.
    pub fn loaded(&self) -> Vec<String> {
        self.shards.keys().cloned().collect()
    }

    /// The shard id covering `point` (what a viewport fetches).
    pub fn shard_of(point: Coord) -> String {
        GeohashStrategy::with_precision(SHARD_PRECISION)
            .locate(Point::new(point.x, point.y))
            .to_string()
    }

    /// The edge-adjacent shard ids of `id` — load these too for seamless
    /// matching near a shard's edges.
    pub fn shard_neighbours(id: &str) -> Result<Vec<String>, String> {
        let hash = Geohash::from_str(id).map_err(|e| e.to_string())?;
        Ok(GeohashStrategy::with_precision(SHARD_PRECISION)
            .neighbours(&hash)
            .iter()
            .map(|neighbour| neighbour.to_string())
            .collect())
    }

    /// Map-match a trajectory (`trace` as WGS84 coords) over the loaded shards.
    pub fn match_trace(
        &self,
        trace: &[Coord],
        opts: Options,
    ) -> Result<RoutedPath<OsmEntryId, OsmEdgeMetadata>, Failure> {
        let line: LineString = trace.iter().copied().collect();

        let context = opts
            .transport
            .map(|transport_mode| TripContext { transport_mode });
        let runtime = <OsmEdgeMetadata as Metadata>::runtime(context);

        let mut match_opts = MatchOptions::<Composite>::default()
            .with_runtime(runtime)
            .with_search_distance(opts.search_distance)
            .with_solver(opts.solver);
        if let Some(reach) = opts.reach_distance {
            let cache =
                PredicateCache::<Composite>::with_reach_distance(Length::new::<meter>(reach));
            match_opts = match_opts.with_cache(Arc::new(cache));
        }

        self.composite
            .r#match(line, match_opts)
            .map_err(Failure::from)
    }

    /// Least-cost route between two points over the loaded shards.
    pub fn route(&self, start: Coord, end: Coord) -> Option<(u32, Vec<Coord>)> {
        let (cost, nodes) = self
            .composite
            .route_points(&Point::new(start.x, start.y), &Point::new(end.x, end.y))?;
        let shape = nodes.iter().map(|node| node.position.into()).collect();
        Some((cost, shape))
    }

    /// The nearest network node to `point`.
    pub fn nearest(&self, point: Coord) -> Option<Coord> {
        self.composite
            .nearest_node(&Point::new(point.x, point.y))
            .map(|node| node.position.into())
    }

    /// The nearest point *on an edge* within `radius` metres of `point`.
    pub fn snap(&self, point: Coord, radius: f64) -> Option<Coord> {
        let origin = Point::new(point.x, point.y);
        let mut projected: Vec<_> = self
            .composite
            .nearest_nodes_projected(&origin, radius)
            .collect();
        projected.sort_by(|(a, _), (b, _)| {
            Haversine
                .distance(origin, *a)
                .partial_cmp(&Haversine.distance(origin, *b))
                .unwrap_or(Ordering::Equal)
        });
        projected.first().map(|(snapped, _)| (*snapped).into())
    }

    /// The network edges within `radius` metres of `point` (a square scan).
    pub fn nearest_edges(&self, point: Coord, radius: f64) -> Vec<Edge<Node<OsmEntryId>>> {
        self.composite
            .edges_at_distance(&Point::new(point.x, point.y), radius)
            .collect()
    }
}

impl From<MatchError> for Failure {
    fn from(error: MatchError) -> Failure {
        match error {
            MatchError::Unanchored(e) => {
                Failure::Unanchored(e.points.iter().map(|p| p.layer as u32).collect())
            }
            MatchError::Disconnected(e) => {
                Failure::Disconnected(e.breaks.iter().map(|b| b.from_layer as u32).collect())
            }
            other => Failure::Solve(other.to_string()),
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use alloc::collections::BTreeSet;
    use geo::LineString;
    use routers_codec::osm::OsmNetwork;
    use routers_fixtures::{SYDNEY, SYNDEY_TRIP, fixture_path};
    use routers_network::edge::Weight;
    use routers_shard::{Selection, SelectionMode, ShardSource};
    use wkt::TryFromWkt;

    /// Adapts an `OsmNetwork` as a shard source (mirrors the generate-shards bin).
    struct OsmSource<'a>(&'a OsmNetwork);
    impl ShardSource<OsmEntryId, OsmEdgeMetadata> for OsmSource<'_> {
        fn nodes<'b>(&'b self) -> Box<dyn Iterator<Item = (OsmEntryId, Point)> + 'b> {
            Box::new(self.0.hash.values().map(|n| (n.id, n.position)))
        }
        fn edges<'b>(
            &'b self,
        ) -> Box<dyn Iterator<Item = (OsmEntryId, OsmEntryId, Weight, OsmEdgeMetadata)> + 'b>
        {
            Box::new(
                self.0
                    .graph
                    .all_edges()
                    .filter_map(|(from, to, (weight, edge_id))| {
                        let meta = self.0.meta.get(&edge_id.index())?.clone();
                        Some((from, to, *weight, meta))
                    }),
            )
        }
    }

    #[test]
    fn matches_a_sydney_trip_across_loaded_shards() {
        let network = OsmNetwork::from_pbf(&fixture_path(SYDNEY)).expect("build network from pbf");
        let strategy = GeohashStrategy::with_precision(SHARD_PRECISION);

        let trip: LineString = LineString::try_from_wkt_str(SYNDEY_TRIP).expect("parse trip");
        let coords: Vec<Coord> = trip.coords().copied().collect();

        // The shards the trip actually crosses.
        let cells: BTreeSet<Geohash> = coords
            .iter()
            .map(|c| strategy.locate(Point::from(*c)))
            .collect();
        assert!(
            cells.len() >= 2,
            "trip should cross >=2 shards at p{SHARD_PRECISION}"
        );

        let source = OsmSource(&network);
        let mut engine = Engine::new();
        for cell in &cells {
            let selection = Selection::new(
                &strategy,
                *cell,
                SelectionMode::OwnedAndPadded {
                    padding_distance: 1000.0,
                },
            );
            let shard = Shard::from_source(&source, &strategy, &selection).expect("build shard");
            let bytes = shard.to_cache_bytes().expect("serialise shard");
            engine
                .load_shard(cell.to_string(), &bytes)
                .expect("load shard");
        }

        assert_eq!(engine.loaded().len(), cells.len());

        let routed = engine
            .match_trace(
                &coords,
                Options {
                    search_distance: Some(50.0),
                    reach_distance: Some(5000.0),
                    solver: SolverVariant::Fastest,
                    transport: None,
                },
            )
            .expect("match succeeds across shards");
        assert!(!routed.discretized.is_empty());

        // Evicting a crossed shard drops the engine below full coverage.
        let first = cells.iter().next().unwrap().to_string();
        assert!(engine.unload_shard(&first));
        assert_eq!(engine.loaded().len(), cells.len() - 1);
    }
}

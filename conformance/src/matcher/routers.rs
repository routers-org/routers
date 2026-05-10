use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use geo::{Coord, LineString};
use routers::r#match::MatchOptions;
use routers::{Match, PredicateCache};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};

use crate::config::RoutersConfig;
use crate::matcher::{MatchResult, Matcher};
use crate::trace::GpsTrace;

pub struct RoutersMatcher {
    graph: OsmNetwork,
    cache: Arc<PredicateCache<OsmEntryId, OsmEdgeMetadata, OsmNetwork>>,

    search_distance: f64,
}

impl RoutersMatcher {
    /// Load the OSM network from the given PBF path.
    ///
    /// Graph loading is done once here, outside of any timed section.
    /// This mirrors how the external services (Valhalla, GraphHopper, FMM)
    /// load their networks at server startup before accepting requests.
    pub fn new(cfg: &RoutersConfig) -> Result<Self> {
        let pbf_path = routers_fixtures::fixture_path(&cfg.network);
        let graph = OsmNetwork::from_pbf(&pbf_path)
            .map_err(|e| anyhow::anyhow!("loading OSM network from {}: {e}", pbf_path.display()))?;

        let cache = Arc::new(PredicateCache::<OsmEntryId, OsmEdgeMetadata, OsmNetwork>::default());

        Ok(Self {
            graph,
            cache,
            search_distance: cfg.search_distance,
        })
    }
}

impl Matcher for RoutersMatcher {
    fn name(&self) -> &str {
        "routers"
    }

    fn match_trace(&self, trace: &GpsTrace) -> Result<MatchResult> {
        let linestring = LineString::new(
            trace
                .points
                .iter()
                .map(|&(lon, lat)| Coord { x: lon, y: lat })
                .collect(),
        );

        let opts = MatchOptions::<OsmEntryId, OsmEdgeMetadata, OsmNetwork>::new()
            .with_search_distance(Some(self.search_distance))
            .with_cache(self.cache.clone());

        let t0 = Instant::now();
        let _ = self
            .graph
            .r#match(linestring, opts)
            .with_context(|| format!("routers failed to match trace '{}'", trace.id))?;
        let duration = t0.elapsed();

        Ok(MatchResult {
            point_count: trace.point_count(),
            duration,
        })
    }
}

use alloc::sync::Arc;

use geo::LineString;
use routers_network::{Metadata, Network};

use crate::{
    candidate::RoutedPath,
    primitives::{MatchError, PredicateCache},
    weigh::SolverVariant,
};

pub const DEFAULT_SEARCH_DISTANCE: f64 = 50.0; // 50m

/// Configuration for a facade [`Match`] call.
///
/// Every option has a default suitable for road-vehicle GPS traces, so
/// [`MatchOptions::default`] is already a complete configuration; the builder
/// methods override just the parts you need:
///
/// ```ignore
/// let opts = MatchOptions::new()
///     .with_search_distance(Some(75.0))
///     .with_cache(cache.clone());
///
/// let routed = network.r#match(linestring, opts)?;
/// ```
#[derive(Clone, Debug)]
pub struct MatchOptions<N: Network> {
    /// The distance the solver will use to search for candidates
    /// around every given input position.
    ///
    /// For positions where accuracy is high, such as the probability
    /// of a candidate being within a lower search distance is above
    /// 3 standard deviations (99.7%), you may lower the value to this
    /// point.
    ///
    /// The default value is [DEFAULT_SEARCH_DISTANCE].
    ///
    /// > Positions that exist at a distance further than this search distance
    /// > may still exist within the result should it be highly probable they
    /// > are the correct position.
    ///
    /// The recommended value range is 25-100m. While higher
    /// values are permitted, and there is no ceiling, a
    /// higher value has a direct impact on the computation time.
    pub search_distance: f64,

    /// An owned instance of the specified runtime for the generic
    /// metadata implementation.
    ///
    /// This value may be obtained by using the associated methods
    /// on a provided runtime implementation, [Metadata::runtime]
    /// or [Metadata::default_runtime].
    ///
    /// Alternatively, it may be created directly if the value and
    /// type are known. This may be particularly useful in a custom
    /// implementation of graph metadata.
    pub runtime: N::Runtime,

    /// The variant of solver to be used by the matcher.
    /// Any given value of the enumeration is accepted,
    pub solver: SolverVariant,

    pub cache: Option<Arc<PredicateCache<N>>>,
}

impl<N: Network> Default for MatchOptions<N> {
    fn default() -> Self {
        Self {
            search_distance: DEFAULT_SEARCH_DISTANCE,
            runtime: <N::Meta>::default_runtime(),
            solver: SolverVariant::default(),
            cache: None,
        }
    }
}

impl<N: Network> MatchOptions<N> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_runtime(self, runtime: N::Runtime) -> Self {
        Self { runtime, ..self }
    }

    pub fn with_cache(self, cache: Arc<PredicateCache<N>>) -> Self {
        Self {
            cache: Some(cache),
            ..self
        }
    }

    pub fn with_solver(self, solver: impl Into<SolverVariant>) -> Self {
        Self {
            solver: solver.into(),
            ..self
        }
    }

    pub fn with_search_distance(self, search_distance: Option<f64>) -> Self {
        Self {
            search_distance: search_distance.unwrap_or(self.search_distance),
            ..self
        }
    }
}

/// For matching a trajectory without assembling a
/// [`Matcher`](crate::Matcher) yourself, use this facade — it is implemented
/// for every [`Network`](routers_network::Network).
///
/// One call builds a matcher from your [`MatchOptions`], runs the batch
/// pipeline, and resolves the result against the network into a
/// [`RoutedPath`]. When the default options suffice, [`MatchSimpleExt`] drops
/// the options argument too.
pub trait Match<N>
where
    N: Network,
{
    /// Matches a given [linestring](LineString) against the map, collapsing
    /// the input onto the network to find the most plausible match for every
    /// input position.
    fn r#match(
        &self,
        linestring: LineString,
        opts: MatchOptions<N>,
    ) -> Result<RoutedPath<N::Entry, N::Meta>, MatchError>;

    /// Snaps a given linestring against the map: each position moved to its
    /// most plausible road position, without routing between them.
    ///
    /// Not yet implemented.
    fn snap(
        &self,
        linestring: LineString,
        opts: MatchOptions<N>,
    ) -> Result<RoutedPath<N::Entry, N::Meta>, MatchError>;
}

/// Simplifies the interface to the `Match` trait, providing methods that uses appropriate options.
pub trait MatchSimpleExt<N>: Match<N>
where
    N: Network,
{
    fn r#match_simple(
        &self,
        linestring: LineString,
    ) -> Result<RoutedPath<N::Entry, N::Meta>, MatchError> {
        self.r#match(linestring, MatchOptions::default())
    }

    fn snap_simple(
        &self,
        linestring: LineString,
    ) -> Result<RoutedPath<N::Entry, N::Meta>, MatchError> {
        self.snap(linestring, MatchOptions::default())
    }
}

impl<T, N: Network> MatchSimpleExt<N> for T where T: Match<N> {}

use std::sync::Arc;

use crate::transition::{MatchError, RoutedPath};
use crate::{PredicateCache, SolverVariant};

use geo::LineString;
use routers_network::{Entry, Metadata, Network};

pub const DEFAULT_SEARCH_DISTANCE: f64 = 50.0; // 50m

pub struct MatchOptions<E: Entry, M: Metadata, N: Network<E, M>> {
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
    pub runtime: M::Runtime,

    /// The variant of solver to be used by the matcher.
    /// Any given value of the enumeration is accepted,
    pub solver: SolverVariant,

    pub cache: Option<Arc<PredicateCache<E, M, N>>>,
}

impl<E: Entry, M: Metadata, N: Network<E, M>> Default for MatchOptions<E, M, N> {
    fn default() -> Self {
        Self {
            search_distance: DEFAULT_SEARCH_DISTANCE,
            runtime: M::default_runtime(),
            solver: SolverVariant::default(),
            cache: None,
        }
    }
}

impl<E: Entry, M: Metadata, N: Network<E, M>> MatchOptions<E, M, N> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_runtime(self, runtime: M::Runtime) -> Self {
        Self { runtime, ..self }
    }

    pub fn with_cache(self, cache: Arc<PredicateCache<E, M, N>>) -> Self {
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

pub trait Match<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    /// Matches a given [linestring](LineString) against the map.
    ///
    /// Matching involves the use of a hidden markov model
    /// using the [`Transition`](crate::Transition) module
    /// to collapse the given input onto the map, finding
    /// appropriate matching for each input value.
    fn r#match(
        &self,
        linestring: LineString,
        opts: MatchOptions<E, M, N>,
    ) -> Result<RoutedPath<E, M>, MatchError>;

    /// Snaps a given linestring against the map.
    ///
    /// TODO: Docs
    fn snap(
        &self,
        linestring: LineString,
        opts: MatchOptions<E, M, N>,
    ) -> Result<RoutedPath<E, M>, MatchError>;
}

/// Simplifies the interface to the `Match` trait, providing methods that uses appropriate options.
pub trait MatchSimpleExt<E, M, N>: Match<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    fn r#match_simple(&self, linestring: LineString) -> Result<RoutedPath<E, M>, MatchError> {
        self.r#match(linestring, MatchOptions::default())
    }

    fn snap_simple(&self, linestring: LineString) -> Result<RoutedPath<E, M>, MatchError> {
        self.snap(linestring, MatchOptions::default())
    }
}

impl<T, E: Entry, M: Metadata, N: Network<E, M>> MatchSimpleExt<E, M, N> for T where
    T: Match<E, M, N>
{
}

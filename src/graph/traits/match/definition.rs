use crate::SolverVariant;
use crate::transition::{MatchError, RoutedPath};

use geo::LineString;
use routers_codec::{Entry, Metadata};

pub const DEFAULT_SEARCH_DISTANCE: f64 = 50.0; // 50m

pub struct MatchOptions<M: Metadata> {
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
}

impl<M: Metadata> Default for MatchOptions<M> {
    fn default() -> Self {
        Self {
            search_distance: DEFAULT_SEARCH_DISTANCE,
            runtime: M::default_runtime(),
            solver: SolverVariant::default(),
        }
    }
}

impl<M: Metadata> MatchOptions<M> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_runtime(self, runtime: M::Runtime) -> Self {
        Self { runtime, ..self }
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

pub trait Match<E, M>
where
    E: Entry,
    M: Metadata,
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
        opts: MatchOptions<M>,
    ) -> Result<RoutedPath<E, M>, MatchError>;

    /// Snaps a given linestring against the map.
    ///
    /// TODO: Docs
    fn snap(
        &self,
        linestring: LineString,
        opts: MatchOptions<M>,
    ) -> Result<RoutedPath<E, M>, MatchError>;
}

/// Simplifies the interface to the `Match` trait, providing methods that uses appropriate options.
pub trait MatchSimpleExt<E, M>: Match<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn r#match_simple(&self, linestring: LineString) -> Result<RoutedPath<E, M>, MatchError> {
        self.r#match(linestring, MatchOptions::default())
    }

    fn snap_simple(&self, linestring: LineString) -> Result<RoutedPath<E, M>, MatchError> {
        self.snap(linestring, MatchOptions::default())
    }
}

impl<T, E: Entry, M: Metadata> MatchSimpleExt<E, M> for T where T: Match<E, M> {}

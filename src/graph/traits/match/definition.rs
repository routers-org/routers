use crate::Solver;
use crate::transition::{MatchError, RoutedPath};

use codec::{Entry, Metadata};
use geo::LineString;

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
        runtime: &M::Runtime,
        solver: impl Solver<E, M>,
        linestring: LineString,
    ) -> Result<RoutedPath<E, M>, MatchError>;

    /// Snaps a given linestring against the map.
    ///
    /// TODO: Docs
    fn snap(
        &self,
        runtime: &M::Runtime,
        solver: impl Solver<E, M>,
        linestring: LineString,
    ) -> Result<RoutedPath<E, M>, MatchError>;
}

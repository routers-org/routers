use crate::Graph;
use crate::Match;
use crate::transition::*;

use geo::LineString;
use log::info;
use routers_codec::{Entry, Metadata};

impl<E, M> Match<E, M> for Graph<E, M>
where
    E: Entry,
    M: Metadata,
{
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = Level::INFO))]
    fn r#match(
        &self,
        runtime: &M::Runtime,
        solver: impl Into<SolverVariant>,
        linestring: LineString,
    ) -> Result<RoutedPath<E, M>, MatchError> {
        info!("Finding matched route for {} positions", linestring.0.len());
        let costing = CostingStrategies::default();

        let solver = solver.into().instance(self.cache.clone());

        // Create our hidden markov model solver
        let transition = Transition::new(self, linestring, costing);

        transition
            .solve(solver, runtime)
            .map(|collapsed| RoutedPath::new(collapsed, self))
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = Level::INFO))]
    fn snap(
        &self,
        _runtime: &M::Runtime,
        _solver: impl Into<SolverVariant>,
        _linestring: LineString,
    ) -> Result<RoutedPath<E, M>, MatchError> {
        unimplemented!()
    }
}

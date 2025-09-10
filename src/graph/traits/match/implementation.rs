use crate::Match;
use crate::transition::*;
use crate::{Graph, MatchOptions};

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
        linestring: LineString,
        opts: MatchOptions<M>,
    ) -> Result<RoutedPath<E, M>, MatchError> {
        info!("Finding matched route for {} positions", linestring.0.len());

        let costing = CostingStrategies::default();

        // Create our hidden markov model solver
        let transition = Transition::new(self, linestring, costing);
        let solver = opts.solver.instance(self.cache.clone());

        transition
            .solve(solver, &opts.runtime)
            .map(|collapsed| RoutedPath::new(collapsed, self))
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = Level::INFO))]
    fn snap(
        &self,
        _linestring: LineString,
        _opts: MatchOptions<M>,
    ) -> Result<RoutedPath<E, M>, MatchError> {
        unimplemented!()
    }
}

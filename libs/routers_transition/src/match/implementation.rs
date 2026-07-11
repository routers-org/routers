use crate::Match;
use crate::Solver;
use crate::candidate::RoutedPath;
use crate::costing::CostingStrategies;
use crate::entity::Transition;
use crate::r#match::MatchOptions;
use crate::primitives::MatchError;

use crate::generation::StandardGenerator;
use geo::LineString;
use log::info;
use routers_network::Network;
use routers_network::{Entry, Metadata};

#[cfg(feature = "tracing")]
use tracing::Level;

impl<T, E: Entry, M: Metadata> Match<E, M, T> for T
where
    T: Network<E, M>,
{
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = Level::INFO))]
    fn r#match(
        &self,
        linestring: LineString,
        opts: MatchOptions<E, M, T>,
    ) -> Result<RoutedPath<E, M>, MatchError> {
        info!("Finding matched route for {} positions", linestring.0.len());

        let costing = CostingStrategies::default();
        let generator = StandardGenerator::new(self, &costing.emission, opts.search_distance);
        let transition = Transition::new(self, linestring, &costing, generator);

        let solver = match opts.cache {
            Some(cache) => opts.solver.instance(cache),
            None => opts.solver.without_cache(),
        };

        // This layer owns the match state; a fresh match starts from an empty one.
        let mut state = crate::MatchState::default();

        solver
            .solve(transition, &opts.runtime, &mut state)
            .map(|collapsed| RoutedPath::new(collapsed, self))
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = Level::INFO))]
    fn snap(
        &self,
        _linestring: LineString,
        _opts: MatchOptions<E, M, T>,
    ) -> Result<RoutedPath<E, M>, MatchError> {
        unimplemented!()
    }
}

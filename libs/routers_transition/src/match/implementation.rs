use crate::candidate::RoutedPath;
use crate::costing::CostingStrategies;
use crate::layer::generation::StandardGenerator;
use crate::r#match::{Match, MatchOptions};
use crate::matcher::Matcher;
use crate::primitives::MatchError;

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

        let weigher = match opts.cache {
            Some(cache) => opts.solver.instance(cache),
            None => opts.solver.without_cache(),
        };

        Matcher::new(self, &costing, generator, weigher, &opts.runtime)
            .r#match(linestring)
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

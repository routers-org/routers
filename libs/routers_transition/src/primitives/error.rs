use thiserror::Error;

/// Why a map-match could not be produced.
#[derive(Error, Debug)]
pub enum MatchError {
    /// The transition graph could not be collapsed into a route.
    #[error("could not collapse transition graph: {0}")]
    CollapseFailure(CollapseError),
}

/// Why collapsing a weighted transition graph into a route failed.
#[derive(Error, Debug)]
pub enum CollapseError {
    /// No path connects the first layer to the last within the graph.
    #[error("could not find a path through the transition graph")]
    NoPathFound,
}

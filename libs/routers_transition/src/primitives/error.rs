use thiserror::Error;

#[derive(Error, Debug)]
pub enum MatchError {
    #[error("no input points were given")]
    NoPointsProvided,

    #[error("could not collapse transition graph: {0}")]
    CollapseFailure(CollapseError),

    #[error("failed to attach ends in transition graph: {0}")]
    EndAttachFailure(EndAttachError),
}

#[derive(Error, Debug)]
pub enum CollapseError {
    #[error("ends were not attached")]
    NoEnds,

    #[error("could not find a path through the transition graph")]
    NoPathFound,
}

#[derive(Error, Debug)]
pub enum EndAttachError {
    #[error("ends already attached to graph, cannot attach more than once")]
    EndsAlreadyAttached,

    #[error("layer missing from graph, both starts and ends must be present")]
    LayerMissing,
}

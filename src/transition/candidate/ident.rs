use petgraph::graph::NodeIndex;

/// The identifier for candidates within the source
/// candidate graph, [`Candidates`].
pub type CandidateId = NodeIndex;

#[derive(Copy, Clone)]
pub struct CandidateRef {
    /// Emission cost of the candidate
    weight: Option<f64>,
}

impl CandidateRef {
    /// Indicates a candidate which exists for routing purposes,
    /// and can only be reached in a resolution step.
    ///
    /// This cannot be called on user-end.
    pub(crate) fn butt() -> Self {
        Self { weight: None }
    }

    /// Creates a standard candidate reference, which contains the
    /// nodes weighting (Derived from the Emission cost).
    pub fn new(weight: f64) -> Self {
        Self {
            weight: Some(weight),
        }
    }

    /// Determines if the candidate was created using [`CandidateId::butt`]
    pub(crate) fn is_butt(&self) -> bool {
        self.weight.is_none()
    }

    pub fn weight(&self) -> f64 {
        self.weight.unwrap_or_default()
    }
}

use petgraph::graph::NodeIndex;

/// TODO: Document
pub trait CandidatePool<C> {
    fn candidate(&self, candidate: &NodeIndex) -> Option<C>;
}

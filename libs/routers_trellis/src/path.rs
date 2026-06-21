use crate::types::NodeId;

/// Result of a solve. `nodes[t]` is the chosen [`NodeId`] in layer `t`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub nodes: Vec<NodeId>,
    pub cost: u32,
    pub reachable: bool,
}

impl Path {
    pub fn new(nodes: Vec<NodeId>, cost: u32, reachable: bool) -> Self {
        Self {
            nodes,
            cost,
            reachable,
        }
    }
}

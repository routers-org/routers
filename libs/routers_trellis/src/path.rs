use crate::types::NodeId;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Result of a solve. `nodes[t]` is the chosen [`NodeId`] in layer `t`.
///
/// A `Path` is always a real, reachable path — an unsolvable trellis is
/// reported as [`SolveError::Unreachable`](crate::SolveError::Unreachable)
/// instead.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Path {
    pub nodes: Vec<NodeId>,
    pub cost: u32,
}

impl Path {
    pub fn new(nodes: Vec<NodeId>, cost: u32) -> Self {
        Self { nodes, cost }
    }
}

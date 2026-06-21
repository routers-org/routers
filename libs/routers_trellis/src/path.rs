type Node = usize;

/// Result of a solve. `nodes[t]` is the chosen node in layer `t`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub nodes: Vec<Node>,
    pub cost: u32,
    pub reachable: bool,
}

impl Path {
    pub fn new(nodes: Vec<Node>, cost: u32, reachable: bool) -> Self {
        Self {
            nodes,
            cost,
            reachable,
        }
    }
}

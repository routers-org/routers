use crate::transition::*;

use pathfinding::num_traits::Zero;
use petgraph::prelude::EdgeRef;
use petgraph::{Directed, Direction, Graph};
use routers_codec::Entry;
use scc::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, RwLock};

type LockedGraph<A, B> = Arc<RwLock<Graph<A, B, Directed>>>;

pub struct Candidates<E>
where
    E: Entry,
{
    /// The locked graph structure storing the candidates
    /// in their layers, connected piecewise.
    ///
    /// The associated node information in the graph can be
    /// used to look up the candidate from the flyweight.
    pub(crate) graph: LockedGraph<CandidateRef, CandidateEdge>,

    /// Candidate flyweight
    pub(crate) lookup: HashMap<CandidateId, Candidate<E>>,

    pub(in crate::transition) source: CandidateId,
    pub(in crate::transition) target: CandidateId,
}

impl<E> Debug for Candidates<E>
where
    E: Entry,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let entries = self.lookup.len();
        write!(
            f,
            "Candidates {{ graph: <locked>, lookup: \"{entries} Entries\" }}"
        )
    }
}

impl<E> Candidates<E>
where
    E: Entry,
{
    /// Returns all the candidates within the following layer to the supplied candidate.
    /// Such as observed in the following diagram, given the candidate exists within
    /// layer `N`, all candidates in layer `N+1`, to which it is connected, will be returned.
    ///
    /// ```text
    ///             Layer    Layer
    ///               N       N+1
    ///
    ///                __/---+
    ///               /
    ///    SOURCE    +-------+
    ///               \
    ///                ‾‾\---+
    /// ```
    pub fn next_layer(&self, candidate: &CandidateId) -> Vec<CandidateId> {
        self.graph
            .read()
            .unwrap()
            .edges_directed(*candidate, Direction::Outgoing)
            .map(|edge| edge.target())
            .collect()
    }

    /// TODO: Provide docs
    pub fn edge(&self, a: &CandidateId, b: &CandidateId) -> Option<CandidateEdge> {
        let reader = self.graph.read().ok()?;

        let edge_index = reader.find_edge(*a, *b)?;

        // TODO: Can we make this operation cheaper? We're cloning vectors internally.
        reader.edge_weight(edge_index).cloned()
    }

    // TODO: Docs
    pub fn attach(&mut self, candidate: CandidateId, layer: &Layer) {
        let mut write_access = self.graph.write().unwrap();
        for node in &layer.nodes {
            write_access.add_edge(candidate, *node, CandidateEdge::zero());
        }
    }

    // TODO: Docs
    pub fn weave(&mut self, layers: &Layers) {
        layers.layers.windows(2).for_each(|layers| {
            if let [a, b] = layers {
                a.nodes.iter().for_each(|node| self.attach(*node, b))
            }
        });
    }

    /// TODO: Provide docs
    pub fn candidate(&self, a: &CandidateId) -> Option<Candidate<E>> {
        self.lookup.get(a).map(|c| *c)
    }
}

impl<E> Default for Candidates<E>
where
    E: Entry,
{
    fn default() -> Self {
        let mut graph = Graph::new();

        let source = graph.add_node(CandidateRef::butt());
        let target = graph.add_node(CandidateRef::butt());

        let graph = Arc::new(RwLock::new(graph));
        let lookup = HashMap::default();

        Candidates {
            graph,
            lookup,

            source,
            target,
        }
    }
}

pub struct Bridge {
    start: CandidateId,
    terminus: CandidateId,
}

pub struct LayeredBridge<'a> {
    bridge: Bridge,

    entering_layer: &'a Layer,
    departing_layer: &'a Layer,
}

impl Bridge {
    pub fn new(start: CandidateId, terminus: CandidateId) -> Self {
        Bridge { start, terminus }
    }

    pub fn layered<'a>(self, entering: &'a Layer, departing: &'a Layer) -> LayeredBridge<'a> {
        LayeredBridge {
            bridge: self,
            entering_layer: entering,
            departing_layer: departing,
        }
    }
}

impl<'a> LayeredBridge<'a> {
    pub fn handle<FnSuccessor: FnMut(&CandidateId) -> Vec<(CandidateId, CandidateEdge)>>(
        &'a self,
        node: &CandidateId,
        mut successors: FnSuccessor,
    ) -> Vec<(CandidateId, CandidateEdge)> {
        // If the node is the same as the bridge's start, it's free to go to the entering layer.
        if *node == self.bridge.start {
            return self
                .entering_layer
                .nodes
                .iter()
                .map(|node| (*node, CandidateEdge::zero()))
                .collect();
        }

        // If the node is within the departing layer, it's free to leave (go to the terminus).
        if self.departing_layer.nodes.contains(node) {
            return vec![(self.bridge.terminus, CandidateEdge::zero())];
        }

        successors(node)
    }
}

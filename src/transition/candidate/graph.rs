use crate::transition::*;

use crate::EndAttachError::{EndsAlreadyAttached, LayerMissing, WriteLockFailed};
use pathfinding::num_traits::{ConstZero, Zero};
use petgraph::algo::astar;
use petgraph::graph::EdgeReference;
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

    ends: Option<(CandidateId, CandidateId)>,
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

    pub fn attach_ends(
        &mut self,
        layers: &Layers,
    ) -> Result<(CandidateId, CandidateId), EndAttachError> {
        if self.ends.is_some() {
            return Err(EndsAlreadyAttached);
        }

        let mut graph = self.graph.write().map_err(|_| WriteLockFailed)?;

        let source = graph.add_node(CandidateRef::butt());
        let target = graph.add_node(CandidateRef::butt());

        // We need to bind the first and last layers to a singular
        // source and target value so we can route toward this given
        // target, from our own source.
        //
        //                   Layer     Layer
        //                     0         N
        //
        //               __/---+   ...   +---\__
        //              /                       \
        //   SOURCE    +-------+   ...   +-------+  TARGET
        //              \                       /
        //               ‾‾\---+   ...   +---/‾‾
        //
        // So, we need to attach each entry within the first/initial layer
        // to this source, and every entry within the last/final layer to
        // the target.

        // Attach the initial layer
        layers
            .first()
            .ok_or(LayerMissing)?
            .nodes
            .iter()
            .for_each(|node| {
                graph.add_edge(source, *node, CandidateEdge::zero());
            });

        // Attach to the final layer
        layers
            .last()
            .ok_or(LayerMissing)?
            .nodes
            .iter()
            .for_each(|node| {
                graph.add_edge(*node, target, CandidateEdge::zero());
            });

        drop(graph);
        let ends = (source, target);
        self.ends = Some(ends);
        Ok(ends)
    }

    /// Collapses transition layers, `layers`, into a single vector of
    /// the finalised points. This is useful for solvers which will
    /// mutate the candidates, and require an external method to solve
    /// based on the calculated edge weights. Iterative solvers which
    /// do not produce a candidate solution do not require this function.
    ///
    /// Takes an owned value to indicate the structure is [terminal].
    ///
    /// [terminal]: Cannot be used again
    pub fn collapse(self) -> Result<CollapsedPath<E>, CollapseError> {
        let (source, target) = self.ends.ok_or(CollapseError::NoEnds)?;

        // There should be exclusive read-access by the time collapse is called.
        // This will block access to any other client using this candidate structure,
        // however this function
        let graph = self
            .graph
            .read()
            .map_err(|_| CollapseError::ReadLockFailed)?;

        let cost_fn = |e: EdgeReference<CandidateEdge>| {
            // Decaying Transition Cost
            let transition_cost = e.weight().weight;

            // Loosely-Decaying Emission Cost
            let emission_cost = graph
                .node_weight(e.target())
                .map_or(u32::MAX, |v| v.weight());

            transition_cost + emission_cost
        };

        let zero = |_| u32::ZERO;

        let (cost, route) = astar(&*graph, source, |node| node == target, cost_fn, zero)
            .ok_or(CollapseError::NoPathFound)?;

        drop(graph);
        Ok(CollapsedPath::new(cost, vec![], route, self))
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
        let graph = Arc::new(RwLock::new(Graph::new()));
        let lookup = HashMap::default();

        Candidates {
            graph,
            lookup,
            ends: None,
        }
    }
}

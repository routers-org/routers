use crate::{
    transition::Transition,
    types::{LayerId, NodeId},
};
use thiserror::Error;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Reserved sentinel meaning "no edge / absent". Real weights must be `<= MAX_WEIGHT`.
pub const NO_EDGE: u32 = u32::MAX;

/// Internal infinity value used in the DP tables. Chosen so that `a + b` for
/// any two values in `0..=INF_W` stays below `u32::MAX`, removing the need for
/// overflow checks inside the hot loop.
pub const INF_W: u32 = 1 << 30;

/// Largest permitted real edge weight (and thus path cost).
pub const MAX_WEIGHT: u32 = INF_W - 1;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TrellisError {
    #[error("trellis is empty")]
    Empty,
    #[error("layer {0} has zero width")]
    ZeroWidthLayer(LayerId),
    #[error("layer out of range: {0}")]
    LayerOutOfRange(LayerId),
    #[error("node out of range: layer={layer} node={node}")]
    NodeOutOfRange { layer: LayerId, node: NodeId },
    #[error("weight too large: {0}")]
    WeightTooLarge(u32),
    #[error("transition length mismatch: layer={layer} expected={expected} got={got}")]
    TransitionLenMismatch {
        layer: LayerId,
        expected: usize,
        got: usize,
    },
    #[error("node weight length mismatch: layer={layer} expected={expected} got={got}")]
    NodeLenMismatch {
        layer: LayerId,
        expected: usize,
        got: usize,
    },
}

type Result<T> = core::result::Result<T, TrellisError>;

/// Layered graph where each layer is connected only to its adjacent layers.
///
/// Each layer-to-layer boundary holds a transition that starts `Pending`
/// (no edges yet) and becomes `Resolved` once edges are written via
/// [`set_edge`](Self::set_edge) or [`fill_transition`](Self::fill_transition).
/// Solvers refuse to run until every
/// transition is resolved.
///
/// Every node additionally carries a weight (default 0), paid on entering it —
/// including in the first layer. Set them with [`fill_nodes`](Self::fill_nodes).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "RawTrellis"))]
pub struct Trellis {
    widths: Vec<u32>,
    nodes: Vec<u32>,
    transitions: Vec<Transition>,
}

/// Hard aggregate bounds for attacker-controlled trellis wire data. These
/// apply before any dense transition buffers are reconstructed or may later be
/// created by resolving a pending boundary.
#[cfg(feature = "serde")]
const MAX_WIRE_LAYERS: usize = 4_096;
#[cfg(feature = "serde")]
const MAX_WIRE_NODES: usize = 1 << 20;
#[cfg(feature = "serde")]
const MAX_WIRE_TOTAL_TRANSITION_SLOTS: usize = 1 << 20;

/// The on-wire shape, intentionally identical to [`Trellis`], but retaining
/// sparse transition wires until aggregate shape validation has completed.
#[cfg(feature = "serde")]
#[derive(Deserialize)]
struct RawTrellis {
    #[serde(deserialize_with = "deserialize_widths")]
    widths: Vec<u32>,
    #[serde(deserialize_with = "deserialize_nodes")]
    nodes: Vec<u32>,
    #[serde(deserialize_with = "deserialize_transitions")]
    transitions: Vec<crate::transition::Wire>,
}

/// Borrowed trellis shape used for serialization after validating that it is
/// accepted by the same bounded wire contract as deserialization.
#[cfg(feature = "serde")]
#[derive(Serialize)]
struct WireTrellis<'a> {
    widths: &'a [u32],
    nodes: &'a [u32],
    transitions: &'a [Transition],
}

#[cfg(feature = "serde")]
fn validate_wire_shape(
    widths: &[u32],
    nodes: &[u32],
    transition_count: usize,
    mut transition_len: impl FnMut(usize) -> Option<usize>,
) -> core::result::Result<(), String> {
    if widths.is_empty() {
        return Err("trellis is empty".to_owned());
    }
    if widths.len() > MAX_WIRE_LAYERS {
        return Err("trellis exceeds the layer limit".to_owned());
    }
    if widths.contains(&0) {
        return Err("trellis contains a zero-width layer".to_owned());
    }
    if transition_count != widths.len() - 1 {
        return Err("trellis transition count does not match layer count".to_owned());
    }

    let node_count = widths
        .iter()
        .try_fold(0usize, |total, &width| total.checked_add(width as usize))
        .ok_or_else(|| "trellis node count overflowed".to_owned())?;
    if node_count > MAX_WIRE_NODES || nodes.len() != node_count {
        return Err("trellis node weights do not match bounded layer shape".to_owned());
    }
    if nodes.iter().any(|&weight| weight > MAX_WEIGHT) {
        return Err("trellis node weight exceeds the maximum".to_owned());
    }

    let mut total_slots = 0usize;
    for layer in 0..transition_count {
        let expected = (widths[layer] as usize)
            .checked_mul(widths[layer + 1] as usize)
            .ok_or_else(|| "trellis transition shape overflowed".to_owned())?;
        total_slots = total_slots
            .checked_add(expected)
            .ok_or_else(|| "trellis transition slots overflowed".to_owned())?;
        if total_slots > MAX_WIRE_TOTAL_TRANSITION_SLOTS {
            return Err("trellis exceeds the aggregate transition slot limit".to_owned());
        }
        if let Some(len) = transition_len(layer)
            && len != expected
        {
            return Err("trellis transition length does not match layer shape".to_owned());
        }
    }
    Ok(())
}

#[cfg(feature = "serde")]
impl Serialize for Trellis {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        validate_wire_shape(
            &self.widths,
            &self.nodes,
            self.transitions.len(),
            |layer| match &self.transitions[layer] {
                Transition::Pending => None,
                Transition::Resolved(weights) => Some(weights.len()),
            },
        )
        .map_err(serde::ser::Error::custom)?;
        WireTrellis {
            widths: &self.widths,
            nodes: &self.nodes,
            transitions: &self.transitions,
        }
        .serialize(serializer)
    }
}

#[cfg(feature = "serde")]
fn deserialize_bounded_vec<'de, D>(
    deserializer: D,
    limit: usize,
    label: &'static str,
) -> core::result::Result<Vec<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedU32s {
        limit: usize,
        label: &'static str,
    }

    impl<'de> serde::de::Visitor<'de> for BoundedU32s {
        type Value = Vec<u32>;

        fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(formatter, "at most {} {}", self.limit, self.label)
        }

        fn visit_seq<A>(self, mut sequence: A) -> core::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                if values.len() == self.limit {
                    return Err(serde::de::Error::custom(format!(
                        "trellis has more than {} {}",
                        self.limit, self.label
                    )));
                }
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedU32s { limit, label })
}

#[cfg(feature = "serde")]
fn deserialize_widths<'de, D>(deserializer: D) -> core::result::Result<Vec<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_WIRE_LAYERS, "layer widths")
}

#[cfg(feature = "serde")]
fn deserialize_nodes<'de, D>(deserializer: D) -> core::result::Result<Vec<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, MAX_WIRE_NODES, "node weights")
}

#[cfg(feature = "serde")]
fn deserialize_transitions<'de, D>(
    deserializer: D,
) -> core::result::Result<Vec<crate::transition::Wire>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedTransitions;

    impl<'de> serde::de::Visitor<'de> for BoundedTransitions {
        type Value = Vec<crate::transition::Wire>;

        fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            formatter.write_str("a bounded trellis transition list")
        }

        fn visit_seq<A>(self, mut sequence: A) -> core::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut transitions = Vec::new();
            while let Some(transition) = sequence.next_element()? {
                if transitions.len() == MAX_WIRE_LAYERS.saturating_sub(1) {
                    return Err(serde::de::Error::custom(format!(
                        "trellis has more than {} transitions",
                        MAX_WIRE_LAYERS - 1
                    )));
                }
                transitions.push(transition);
            }
            Ok(transitions)
        }
    }

    deserializer.deserialize_seq(BoundedTransitions)
}

#[cfg(feature = "serde")]
impl TryFrom<RawTrellis> for Trellis {
    type Error = String;

    fn try_from(raw: RawTrellis) -> core::result::Result<Self, Self::Error> {
        validate_wire_shape(&raw.widths, &raw.nodes, raw.transitions.len(), |layer| {
            crate::transition::Transition::wire_len(&raw.transitions[layer]).map(|len| len as usize)
        })?;

        let transitions = raw
            .transitions
            .into_iter()
            .map(crate::transition::Transition::from_wire::<serde::de::value::Error>)
            .collect::<core::result::Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            widths: raw.widths,
            nodes: raw.nodes,
            transitions,
        })
    }
}

impl Trellis {
    /// New trellis with the given per-layer node counts. All transitions start
    /// `Pending`; all node weights start 0.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(level = "debug", skip(widths), fields(layers = widths.len()))
    )]
    pub fn new(widths: Vec<u32>) -> Result<Self> {
        if widths.is_empty() {
            return Err(TrellisError::Empty);
        }
        if let Some(l) = widths.iter().position(|&w| w == 0) {
            return Err(TrellisError::ZeroWidthLayer(LayerId(l as u32)));
        }

        let transitions = widths.windows(2).map(|_| Transition::Pending).collect();
        let nodes = vec![0; widths.iter().sum::<u32>() as usize];
        let t = Trellis {
            widths,
            nodes,
            transitions,
        };

        log::debug!(
            "trellis created: {} layers, widths={:?}",
            t.layers(),
            t.widths()
        );
        Ok(t)
    }

    /// Number of layers (node columns, not transitions).
    #[inline]
    pub fn layers(&self) -> usize {
        self.widths.len()
    }

    /// The layer-to-layer boundaries, each identified by its lower [`LayerId`]
    /// (boundary `k` connects layer `k` to layer `k+1`).
    pub fn boundaries(&self) -> impl DoubleEndedIterator<Item = LayerId> + ExactSizeIterator {
        (0..self.transitions.len()).map(|k| LayerId(k as u32))
    }

    /// Per-layer node counts.
    #[inline]
    pub fn widths(&self) -> &[u32] {
        &self.widths
    }

    /// Each layer's node range within a flat, layer-major buffer holding every
    /// node in the trellis (the layout solvers use for their DP tables).
    pub fn layer_ranges(&self) -> impl Iterator<Item = core::ops::Range<usize>> + '_ {
        self.widths.iter().scan(0, |start, &width| {
            let range = *start..*start + width as usize;
            *start = range.end;
            Some(range)
        })
    }

    /// Whether the transition from `layer` to `layer+1` is resolved.
    #[inline]
    pub fn is_resolved(&self, layer: LayerId) -> bool {
        self.transitions
            .get(layer.index())
            .map(Transition::is_resolved)
            .unwrap_or(false)
    }

    /// `true` if every transition is resolved (safe to solve).
    pub fn fully_resolved(&self) -> bool {
        self.transitions.iter().all(Transition::is_resolved)
    }

    /// Index of the first `Pending` transition, or `None` if all resolved.
    pub fn first_pending(&self) -> Option<LayerId> {
        self.transitions
            .iter()
            .position(|t| !t.is_resolved())
            .map(|i| LayerId(i as u32))
    }

    /// Reset a transition back to `Pending`, discarding its edges.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip(self)))]
    pub fn mark_pending(&mut self, layer: LayerId) -> Result<()> {
        if layer.index() >= self.transitions.len() {
            return Err(TrellisError::LayerOutOfRange(layer));
        }
        self.transitions[layer.index()] = Transition::Pending;
        log::debug!("mark_pending: L{layer}");
        Ok(())
    }

    /// Raw edge weights for a transition, row-major `[from * next_width + to]`.
    /// Returns `None` if the transition is still `Pending`.
    pub fn layer(&self, layer: LayerId) -> Option<&[u32]> {
        self.transitions.get(layer.index())?.weights()
    }

    /// Every boundary left `Pending` — unresolved because nothing bridged it —
    /// in layer order. These are the trajectory's gaps: stretches the weigher
    /// could not cross at all. Empty when every boundary resolved.
    pub fn disconnections(&self) -> Vec<LayerId> {
        self.transitions
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.is_resolved())
            .map(|(i, _)| LayerId(i as u32))
            .collect()
    }

    /// Weight of a single edge across the `layer → layer+1` boundary.
    /// Returns `INF_W` if the transition is pending or the edge is absent.
    pub fn edge_weight(&self, layer: LayerId, from: NodeId, to: NodeId) -> u32 {
        let next_width = self.widths[layer.index() + 1] as usize;
        self.transitions[layer.index()]
            .weights()
            .map(|w| w[from.index() * next_width + to.index()])
            .unwrap_or(INF_W)
    }

    /// Set one directed edge `from → to` across the `layer → layer+1` boundary.
    ///
    /// If the transition was `Pending`, it becomes `Resolved` with all other
    /// edges initialised to absent (`INF_W`).
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip(self)))]
    pub fn set_edge(
        &mut self,
        layer: LayerId,
        from: NodeId,
        to: NodeId,
        weight: u32,
    ) -> Result<()> {
        if weight > MAX_WEIGHT {
            return Err(TrellisError::WeightTooLarge(weight));
        }

        let layer_idx = layer.index();
        let next_idx = layer_idx + 1;
        if next_idx >= self.widths.len() {
            return Err(TrellisError::LayerOutOfRange(layer));
        }

        let cur_width = self.widths[layer_idx] as usize;
        let next_width = self.widths[next_idx] as usize;

        if from.index() >= cur_width {
            return Err(TrellisError::NodeOutOfRange { layer, node: from });
        }
        if to.index() >= next_width {
            return Err(TrellisError::NodeOutOfRange {
                layer: LayerId(next_idx as u32),
                node: to,
            });
        }

        self.transitions[layer_idx].ensure_resolved(cur_width * next_width);
        self.transitions[layer_idx].weights_mut().unwrap()
            [from.index() * next_width + to.index()] = weight;

        log::debug!("set_edge: L{layer} {from}→{to} w={weight}");
        Ok(())
    }

    /// Allocates a new layer with the given width, a pending transition into it,
    /// and zero node weights. Returns the new layer's id.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip(self)))]
    pub fn add_layer(&mut self, width: u32) -> Result<LayerId> {
        let id = LayerId(self.widths.len() as u32);
        if width == 0 {
            return Err(TrellisError::ZeroWidthLayer(id));
        }

        self.widths.push(width);
        self.nodes.extend(core::iter::repeat_n(0, width as usize));
        self.transitions.push(Transition::Pending);

        Ok(id)
    }

    /// The id of the most recent layer. Always valid: [`new`](Self::new)
    /// rejects zero layers and nothing removes them.
    pub fn last_id(&self) -> LayerId {
        LayerId(self.widths.len() as u32 - 1)
    }

    /// Bulk-fill a transition, row-major `[from * next_width + to]`.
    ///
    /// Entries equal to `NO_EDGE` are stored as absent; all other entries must
    /// be `<= MAX_WEIGHT`. Replaces any previous edge data (pending or resolved).
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(level = "debug", skip(self, rows), fields(edges = rows.len()))
    )]
    pub fn fill_transition(&mut self, layer: LayerId, rows: &[u32]) -> Result<()> {
        let layer_idx = layer.index();
        if layer_idx + 1 >= self.widths.len() {
            return Err(TrellisError::LayerOutOfRange(layer));
        }

        let expected = (self.widths[layer_idx] as usize) * (self.widths[layer_idx + 1] as usize);
        if rows.len() != expected {
            return Err(TrellisError::TransitionLenMismatch {
                layer,
                expected,
                got: rows.len(),
            });
        }

        for &w in rows {
            if w != NO_EDGE && w > MAX_WEIGHT {
                return Err(TrellisError::WeightTooLarge(w));
            }
        }

        let weights: Vec<u32> = rows
            .iter()
            .map(|&w| if w == NO_EDGE { INF_W } else { w })
            .collect();
        self.transitions[layer_idx] = Transition::Resolved(weights);

        log::debug!("fill_transition: L{layer} ({} edges)", rows.len());
        Ok(())
    }

    /// The flat range of `layer` within the layer-major node buffer.
    fn node_range(&self, layer: LayerId) -> Result<core::ops::Range<usize>> {
        self.layer_ranges()
            .nth(layer.index())
            .ok_or(TrellisError::LayerOutOfRange(layer))
    }

    /// Fill a layer's node weights (one per node, each `<= MAX_WEIGHT`).
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(level = "debug", skip(self, weights), fields(nodes = weights.len()))
    )]
    pub fn fill_nodes(&mut self, layer: LayerId, weights: &[u32]) -> Result<()> {
        let range = self.node_range(layer)?;
        if weights.len() != range.len() {
            return Err(TrellisError::NodeLenMismatch {
                layer,
                expected: range.len(),
                got: weights.len(),
            });
        }
        if let Some(&w) = weights.iter().find(|&&w| w > MAX_WEIGHT) {
            return Err(TrellisError::WeightTooLarge(w));
        }

        self.nodes[range].copy_from_slice(weights);
        Ok(())
    }

    /// A layer's node weights, one per node.
    pub fn node_weights(&self, layer: LayerId) -> Option<&[u32]> {
        self.node_range(layer).ok().map(|range| &self.nodes[range])
    }

    /// One node's weight, or `None` when out of range.
    pub fn node_weight(&self, layer: LayerId, node: NodeId) -> Option<u32> {
        self.node_weights(layer)?.get(node.index()).copied()
    }

    /// Every node weight, layer-major — positioned by [`layer_ranges`](Self::layer_ranges).
    pub(crate) fn node_table(&self) -> &[u32] {
        &self.nodes
    }

    /// Total cost of a full node-path: the first node's weight, then each
    /// boundary's edge weight plus the entered node's weight, saturating at
    /// the DP tables' infinity.
    ///
    /// Panics if `nodes` does not name one valid node per layer.
    pub fn path_cost(&self, nodes: &[NodeId]) -> u32 {
        let node_cost = |layer: usize, node: NodeId| {
            self.node_weight(LayerId(layer as u32), node)
                .expect("path names one valid node per layer")
        };

        let mut cost = node_cost(0, nodes[0]);
        for (layer, hop) in nodes.windows(2).enumerate() {
            let edge = self.edge_weight(LayerId(layer as u32), hop[0], hop[1]);
            cost = cost
                .saturating_add(edge)
                .saturating_add(node_cost(layer + 1, hop[1]));
        }
        cost
    }

    /// An owned copy of the layers in `range`, keeping their node weights and
    /// interior transitions. Boundary states carry over; the transitions that
    /// crossed the cut are dropped.
    pub fn partition(&self, range: core::ops::Range<LayerId>) -> Result<Trellis> {
        let (start, end) = (range.start.index(), range.end.index());
        if start >= end {
            return Err(TrellisError::Empty);
        }
        if end > self.layers() {
            return Err(TrellisError::LayerOutOfRange(range.end));
        }

        let node_start = self.node_range(range.start)?.start;
        let node_end = self.node_range(LayerId(end as u32 - 1))?.end;

        Ok(Trellis {
            widths: self.widths[start..end].to_vec(),
            nodes: self.nodes[node_start..node_end].to_vec(),
            transitions: self.transitions[start..end - 1].to_vec(),
        })
    }

    /// An owned copy of the last `n` layers — the windowing primitive for
    /// bounding a growing trellis. `n` is clamped to the layer count.
    pub fn last(&self, n: usize) -> Result<Trellis> {
        let start = self.layers().saturating_sub(n);
        self.partition(LayerId(start as u32)..LayerId(self.layers() as u32))
    }
}

#[cfg(all(test, feature = "serde"))]
mod deserialize_tests {
    use super::*;

    #[derive(Serialize)]
    #[allow(dead_code)]
    enum TestWire {
        Pending,
        Sparse { len: u32, edges: Vec<(u32, u32)> },
    }

    #[derive(Serialize)]
    struct TestTrellis {
        widths: Vec<u32>,
        nodes: Vec<u32>,
        transitions: Vec<TestWire>,
    }

    #[test]
    fn aggregate_transition_slots_are_rejected_before_dense_reconstruction() {
        // Each empty sparse boundary would individually reconstruct to 4 MiB.
        // The aggregate validation rejects this two-boundary wire before either
        // dense buffer is allocated.
        let wire = TestTrellis {
            widths: vec![1024, 1024, 1024],
            nodes: vec![0; 3 * 1024],
            transitions: vec![
                TestWire::Sparse {
                    len: MAX_WIRE_TOTAL_TRANSITION_SLOTS as u32,
                    edges: Vec::new(),
                },
                TestWire::Sparse {
                    len: MAX_WIRE_TOTAL_TRANSITION_SLOTS as u32,
                    edges: Vec::new(),
                },
            ],
        };
        let bytes = postcard::to_allocvec(&wire).expect("forged wire serializes");
        assert!(postcard::from_bytes::<Trellis>(&bytes).is_err());
    }

    #[test]
    fn pending_boundary_with_excessive_potential_slots_is_rejected() {
        let width = (MAX_WIRE_NODES / 2) as u32;
        let wire = TestTrellis {
            widths: vec![width, width],
            nodes: vec![0; MAX_WIRE_NODES],
            transitions: vec![TestWire::Pending],
        };
        let bytes = postcard::to_allocvec(&wire).expect("forged wire serializes");
        assert!(postcard::from_bytes::<Trellis>(&bytes).is_err());
    }

    #[test]
    fn serialization_rejects_a_trellis_over_the_aggregate_transition_limit() {
        let mut trellis = Trellis::new(vec![1024, 1024, 1024]).expect("valid trellis");
        let absent_edges = vec![NO_EDGE; MAX_WIRE_TOTAL_TRANSITION_SLOTS];
        trellis
            .fill_transition(LayerId(0), &absent_edges)
            .expect("first boundary is valid");
        trellis
            .fill_transition(LayerId(1), &absent_edges)
            .expect("second boundary is valid");

        assert!(postcard::to_allocvec(&trellis).is_err());
    }

    #[test]
    fn serialization_rejects_pending_trellis_with_excessive_potential_slots() {
        let width = (MAX_WIRE_NODES / 2) as u32;
        let trellis = Trellis::new(vec![width, width]).expect("valid trellis");
        assert!(postcard::to_allocvec(&trellis).is_err());
    }

    #[test]
    fn trellis_round_trip_preserves_the_existing_wire_shape() {
        let trellis = Trellis::new(vec![2, 3]).expect("valid trellis");
        let bytes = postcard::to_allocvec(&trellis).expect("trellis serializes");
        assert_eq!(postcard::from_bytes::<Trellis>(&bytes).unwrap(), trellis);
    }

    #[test]
    fn wire_rejects_node_weight_above_the_solver_limit() {
        let wire = TestTrellis {
            widths: vec![1],
            nodes: vec![MAX_WEIGHT + 1],
            transitions: Vec::new(),
        };
        let bytes = postcard::to_allocvec(&wire).expect("forged wire serializes");
        assert!(postcard::from_bytes::<Trellis>(&bytes).is_err());
    }
}

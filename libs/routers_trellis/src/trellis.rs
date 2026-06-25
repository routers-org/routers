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
}

type Result<T> = std::result::Result<T, TrellisError>;

/// Layered graph where each layer is connected only to its adjacent layers.
///
/// Each layer-to-layer boundary holds a [`Transition`] that starts `Pending`
/// (no edges yet) and becomes `Resolved` once edges are written via
/// [`set_edge`] or [`fill_transition`]. Solvers refuse to run until every
/// transition is resolved.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Trellis {
    widths: Vec<u32>,
    transitions: Vec<Transition>,
}

impl Trellis {
    /// New trellis with the given per-layer node counts. All transitions start `Pending`.
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
        let t = Trellis {
            widths,
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

    /// Per-layer node counts.
    #[inline]
    pub fn widths(&self) -> &[u32] {
        &self.widths
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

    /// Allocates a new layer with the given width and pending transition.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip(self)))]
    pub fn add_layer(&mut self, width: u32) -> Result<()> {
        self.widths.push(width);
        self.transitions.push(Transition::Pending);

        Ok(())
    }

    /// Resolves the transition for the given layer with the given rows of weights.
    /// Fixed weights, row-major [from * next_width + to]; absent edges = INF_W.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip(self)))]
    pub fn fill_layer(&mut self, layer: LayerId, rows: Vec<u32>) -> Result<()> {
        if let Some(transition) = self.transitions.get_mut(layer.index()) {
            *transition = Transition::Resolved(rows);
        } else {
            return Err(TrellisError::LayerOutOfRange(layer));
        }

        Ok(())
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
}

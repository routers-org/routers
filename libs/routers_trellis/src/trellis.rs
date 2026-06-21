use crate::transition::Transition;
use thiserror::Error;

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
    ZeroWidthLayer(usize),
    #[error("layer out of range: {0}")]
    LayerOutOfRange(usize),
    #[error("node out of range: layer={layer} node={node}")]
    NodeOutOfRange { layer: usize, node: usize },
    #[error("weight too large: {0}")]
    WeightTooLarge(u32),
    #[error("transition length mismatch: layer={layer} expected={expected} got={got}")]
    TransitionLenMismatch {
        layer: usize,
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
pub struct Trellis {
    widths: Vec<usize>,
    transitions: Vec<Transition>,
}

impl Trellis {
    /// New trellis with the given per-layer node counts. All transitions start `Pending`.
    pub fn new(widths: Vec<usize>) -> Result<Self> {
        if widths.is_empty() {
            return Err(TrellisError::Empty);
        }
        if let Some(l) = widths.iter().position(|&w| w == 0) {
            return Err(TrellisError::ZeroWidthLayer(l));
        }

        let transitions = widths.windows(2).map(|_| Transition::Pending).collect();

        Ok(Trellis {
            widths,
            transitions,
        })
    }

    /// Number of layers (nodes, not transitions).
    #[inline]
    pub fn layers(&self) -> usize {
        self.widths.len()
    }

    /// Per-layer node counts.
    #[inline]
    pub fn widths(&self) -> &[usize] {
        &self.widths
    }

    /// Whether the transition from `layer` to `layer+1` is resolved.
    #[inline]
    pub fn is_resolved(&self, layer: usize) -> bool {
        self.transitions
            .get(layer)
            .map(Transition::is_resolved)
            .unwrap_or(false)
    }

    /// `true` if every transition is resolved (safe to solve).
    pub fn fully_resolved(&self) -> bool {
        self.transitions.iter().all(Transition::is_resolved)
    }

    /// Index of the first `Pending` transition, or `None` if all resolved.
    pub fn first_pending(&self) -> Option<usize> {
        self.transitions.iter().position(|t| !t.is_resolved())
    }

    /// Reset a resolved transition back to `Pending`, clearing its edges.
    pub fn mark_pending(&mut self, layer: usize) -> Result<()> {
        if layer >= self.transitions.len() {
            return Err(TrellisError::LayerOutOfRange(layer));
        }
        self.transitions[layer] = Transition::Pending;
        Ok(())
    }

    /// Raw edge weights for a transition, row-major `[from * next_width + to]`.
    /// Returns `None` if the transition is still `Pending`.
    pub fn layer(&self, layer: usize) -> Option<&[u32]> {
        self.transitions.get(layer)?.weights()
    }

    /// Weight of a single edge across the `layer → layer+1` boundary.
    /// Returns `INF_W` if the transition is pending or the edge is absent.
    pub fn edge_weight(&self, layer: usize, from: usize, to: usize) -> u32 {
        let next_width = self.widths[layer + 1];
        self.transitions[layer]
            .weights()
            .map(|w| w[from * next_width + to])
            .unwrap_or(INF_W)
    }

    /// Set one directed edge `from → to` across the `layer → layer+1` boundary.
    ///
    /// If the transition was `Pending`, it becomes `Resolved` with all other
    /// edges initialised to absent (`INF_W`).
    pub fn set_edge(&mut self, layer: usize, from: usize, to: usize, weight: u32) -> Result<()> {
        if weight > MAX_WEIGHT {
            return Err(TrellisError::WeightTooLarge(weight));
        }

        let next_layer = layer + 1;
        if next_layer >= self.widths.len() {
            return Err(TrellisError::LayerOutOfRange(layer));
        }

        let (cur_width, next_width) = (self.widths[layer], self.widths[next_layer]);
        if from >= cur_width {
            return Err(TrellisError::NodeOutOfRange { layer, node: from });
        }
        if to >= next_width {
            return Err(TrellisError::NodeOutOfRange {
                layer: next_layer,
                node: to,
            });
        }

        self.transitions[layer].ensure_resolved(cur_width * next_width);
        self.transitions[layer].weights_mut().unwrap()[from * next_width + to] = weight;
        Ok(())
    }

    /// Bulk-fill a transition, row-major `[from * next_width + to]`.
    ///
    /// Entries equal to `NO_EDGE` are stored as absent; all other entries must
    /// be `<= MAX_WEIGHT`. Replaces any previous edge data (pending or resolved).
    pub fn fill_transition(&mut self, layer: usize, rows: &[u32]) -> Result<()> {
        if layer + 1 >= self.widths.len() {
            return Err(TrellisError::LayerOutOfRange(layer));
        }

        let expected = self.widths[layer] * self.widths[layer + 1];
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
        self.transitions[layer] = Transition::Resolved(weights);
        Ok(())
    }
}

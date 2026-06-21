use thiserror::Error;

/// Reserved sentinel meaning "no edge". Real weights must be `<= MAX_WEIGHT`.
pub const NO_EDGE: u32 = u32::MAX;

/// Internal unreachable/no-edge value. Chosen so `a + b` for any two values in
/// `0..=INF_W` stays below `u32::MAX`, so the hot loop needs no overflow checks.
pub const INF_W: u32 = 1 << 30;

/// Largest permitted real edge weight (and thus path cost).
const MAX_WEIGHT: u32 = INF_W - 1;

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

// Layered graph, where each layer is connected only to it's adjacent layers.
#[derive(Clone, Debug)]
pub struct Trellis {
    // The widths of each layer, indexable as `widths[layer]`.
    widths: Vec<usize>,

    // Row-Major transition representation.
    // I.e. access as `transitions[t][from * widths[t+1] + to]`.
    // Absent edges hold `INF_W`.
    transitions: Vec<Vec<u32>>,
}

impl Trellis {
    /// New trellis with the given per-layer node counts and no edges yet.
    pub fn new(widths: Vec<usize>) -> Result<Self> {
        if widths.is_empty() {
            return Err(TrellisError::Empty);
        }

        if let Some(l) = widths.iter().position(|&w| w == 0) {
            return Err(TrellisError::ZeroWidthLayer(l));
        }

        let trans = widths
            .windows(2)
            .map(|w| vec![INF_W; w[0] * w[1]])
            .collect();

        Ok(Trellis {
            widths,
            transitions: trans,
        })
    }

    #[inline]
    pub fn layers(&self) -> usize {
        self.widths.len()
    }

    #[inline]
    pub fn widths(&self) -> &[usize] {
        &self.widths
    }

    pub fn layer(&self, layer: usize) -> &[u32] {
        &self.transitions[layer]
    }

    /// Set one directed edge `from -> to` across the `layer -> layer+1` boundary.
    pub fn set_edge(&mut self, layer: usize, from: usize, to: usize, weight: u32) -> Result<()> {
        if weight > MAX_WEIGHT {
            return Err(TrellisError::WeightTooLarge(weight));
        }

        // Grab the adjacent layers, and check for validity.
        let (l1, l2) = (layer, layer + 1);
        if l2 >= self.widths.len() {
            return Err(TrellisError::LayerOutOfRange(layer));
        }

        match (self.widths[l1], self.widths[l2]) {
            (fw, _) if from >= fw => Err(TrellisError::NodeOutOfRange {
                layer: l1,
                node: from,
            }),
            (_, tw) if to >= tw => Err(TrellisError::NodeOutOfRange {
                layer: l2,
                node: to,
            }),
            (_, tw) => {
                self.transitions[l1][from * tw + to] = weight;
                Ok(())
            }
        }
    }

    /// Bulk-fill a transition, row-major `[from * next_width + to]`.
    ///
    /// Entries with value `NO_EDGE` are stored as absent; all other entries must be `<= MAX_WEIGHT`.
    pub fn insert_transitions(&mut self, layer: usize, rows: &[u32]) -> Result<()> {
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

        let dst = &mut self.transitions[layer];
        for (d, &w) in dst.iter_mut().zip(rows) {
            *d = if w == NO_EDGE { INF_W } else { w };
        }

        Ok(())
    }
}

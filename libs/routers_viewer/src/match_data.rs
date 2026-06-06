use geo::{Coord, LineString};

/// One candidate snap position considered during matching for a single GPS layer.
#[derive(Clone, Debug)]
pub struct CandidateViz {
    pub position: Coord,
    pub emission: u32,
}

/// All visualisation data for one GPS input point (one "layer" in the HMM).
#[derive(Clone, Debug)]
pub struct LayerViz {
    /// The original GPS coordinate.
    pub original: Coord,
    /// Every candidate snap position, in HashMap iteration order.
    pub candidates: Vec<CandidateViz>,
    /// Index into `candidates` of the solver-chosen snap. `None` only if the
    /// solver produced an empty route for this layer (should not happen).
    pub chosen_idx: Option<usize>,
}

/// All visualisation data derived from a successful match.
/// Computed once at match time; plugins consume slices of it each frame.
#[derive(Clone, Debug)]
pub struct MatchData {
    /// Viterbi cost — lower is more confident.
    pub cost: u32,
    /// The raw GPS input trace.
    pub original_line: LineString,
    /// Full road-geometry interpolation of the matched path.
    pub interpolated_line: LineString,
    /// One entry per GPS input point.
    pub layers: Vec<LayerViz>,
    /// Coordinate sequence for each chosen transition between consecutive
    /// layers (`transitions[i]` is the road path from layer i to layer i+1).
    pub transitions: Vec<Vec<Coord>>,
}

impl MatchData {
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }
}

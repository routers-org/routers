use std::time::Duration;

use geo::{Coord, LineString};

#[derive(Clone, Debug)]
pub struct MatchCandidate {
    pub position: Coord,
    pub emission: u32,
}

#[derive(Clone, Debug)]
pub struct MatchLayer {
    pub original: Coord,
    pub candidates: Vec<MatchCandidate>,
    pub chosen_idx: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct MatchData {
    pub cost: u32,
    pub time: Duration,
    pub original_line: LineString,
    pub interpolated_line: LineString,
    pub layers: Vec<MatchLayer>,
}

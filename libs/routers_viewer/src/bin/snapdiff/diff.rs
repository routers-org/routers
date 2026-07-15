use std::ops::Range;

use geo::{Distance, Haversine, LineString, Point};
use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::parse::Snapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Removed,
    Modified,
    Unchanged,
}

impl Status {
    pub fn badge(self) -> &'static str {
        match self {
            Status::Added => "A",
            Status::Removed => "R",
            Status::Modified => "M",
            Status::Unchanged => "=",
        }
    }

    pub fn changed(self) -> bool {
        self != Status::Unchanged
    }
}

pub struct FixtureDiff {
    /// Display name, e.g. `LAX_LYNWOOD` from `map_match__LAX_LYNWOOD_coords.snap`.
    pub name: String,
    pub status: Status,
    pub base: Option<LineString<f64>>,
    pub head: Option<LineString<f64>>,
    /// Point-index ranges in `base` that were removed or replaced.
    pub base_spans: Vec<Range<usize>>,
    /// Point-index ranges in `head` that were inserted or replaced.
    pub head_spans: Vec<Range<usize>>,
    pub points_removed: usize,
    pub points_added: usize,
    /// How far the geometry moved: the largest haversine distance (metres)
    /// from any changed point to the nearest point on the other side's line.
    pub magnitude_m: f64,
    /// A parse failure on either side; the fixture is still listed.
    pub error: Option<String>,
}

impl FixtureDiff {
    pub fn compute(name: String, base: Option<Snapshot>, head: Option<Snapshot>) -> Self {
        let status = match (&base, &head) {
            (None, Some(_)) => Status::Added,
            (Some(_), None) => Status::Removed,
            (Some(b), Some(h)) if b.lines == h.lines => Status::Unchanged,
            _ => Status::Modified,
        };

        let (base_spans, head_spans) = match (&base, &head) {
            (Some(b), Some(h)) if status == Status::Modified => changed_spans(&b.lines, &h.lines),
            _ => Default::default(),
        };

        let points_removed = base_spans.iter().map(Range::len).sum();
        let points_added = head_spans.iter().map(Range::len).sum();

        let base_line = base.map(|s| s.line_string);
        let head_line = head.map(|s| s.line_string);

        let magnitude_m = match (&base_line, &head_line) {
            (Some(b), Some(h)) => magnitude(b, &base_spans, h)
                .max(magnitude(h, &head_spans, b)),
            _ => 0.0,
        };

        Self {
            name,
            status,
            base: base_line,
            head: head_line,
            base_spans,
            head_spans,
            points_removed,
            points_added,
            magnitude_m,
            error: None,
        }
    }

    pub fn parse_error(name: String, error: String) -> Self {
        Self {
            name,
            status: Status::Modified,
            base: None,
            head: None,
            base_spans: Vec::new(),
            head_spans: Vec::new(),
            points_removed: 0,
            points_added: 0,
            magnitude_m: 0.0,
            error: Some(error),
        }
    }
}

/// Sequence-diff the coordinate lines of both snapshots, returning the
/// contiguous changed ranges on each side.
fn changed_spans(base: &[String], head: &[String]) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut base_spans = Vec::new();
    let mut head_spans = Vec::new();

    for op in capture_diff_slices(Algorithm::Myers, base, head) {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index, old_len, ..
            } => base_spans.push(old_index..old_index + old_len),
            DiffOp::Insert {
                new_index, new_len, ..
            } => head_spans.push(new_index..new_index + new_len),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                base_spans.push(old_index..old_index + old_len);
                head_spans.push(new_index..new_index + new_len);
            }
        }
    }

    (base_spans, head_spans)
}

/// Max over changed points in `from` of the haversine distance to the nearest
/// point of `to` — a metres-scale answer to "how far did the match move?".
fn magnitude(from: &LineString<f64>, spans: &[Range<usize>], to: &LineString<f64>) -> f64 {
    if to.0.is_empty() {
        return 0.0;
    }

    spans
        .iter()
        .flat_map(|r| &from.0[r.start..r.end.min(from.0.len())])
        .map(|c| {
            to.0.iter()
                .map(|o| Haversine.distance(Point::from(*c), Point::from(*o)))
                .fold(f64::INFINITY, f64::min)
        })
        .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_coords_snap;

    fn snap(coords: &[(f64, f64)]) -> Snapshot {
        let body: String = coords
            .iter()
            .map(|(lon, lat)| format!("{lon} {lat}\n"))
            .collect();
        parse_coords_snap(&format!("---\nsource: x\n---\n{body}")).unwrap()
    }

    #[test]
    fn identical_is_unchanged() {
        let d = FixtureDiff::compute(
            "t".into(),
            Some(snap(&[(1.0, 1.0), (2.0, 2.0)])),
            Some(snap(&[(1.0, 1.0), (2.0, 2.0)])),
        );
        assert_eq!(d.status, Status::Unchanged);
        assert!(d.base_spans.is_empty() && d.head_spans.is_empty());
    }

    #[test]
    fn missing_sides_are_added_removed() {
        let a = FixtureDiff::compute("t".into(), None, Some(snap(&[(1.0, 1.0)])));
        assert_eq!(a.status, Status::Added);
        let r = FixtureDiff::compute("t".into(), Some(snap(&[(1.0, 1.0)])), None);
        assert_eq!(r.status, Status::Removed);
    }

    #[test]
    fn single_point_shift_yields_one_span_each_side() {
        let d = FixtureDiff::compute(
            "t".into(),
            Some(snap(&[(1.0, 1.0), (2.0, 2.0), (3.0, 3.0)])),
            Some(snap(&[(1.0, 1.0), (2.0, 2.5), (3.0, 3.0)])),
        );
        assert_eq!(d.status, Status::Modified);
        assert_eq!(d.base_spans, vec![1..2]);
        assert_eq!(d.head_spans, vec![1..2]);
        assert_eq!(d.points_removed, 1);
        assert_eq!(d.points_added, 1);
        // 0.5° of latitude ≈ 55.6 km.
        assert!((d.magnitude_m - 55_600.0).abs() < 1_000.0, "{}", d.magnitude_m);
    }

    #[test]
    fn insertion_only_touches_head_spans() {
        let d = FixtureDiff::compute(
            "t".into(),
            Some(snap(&[(1.0, 1.0), (3.0, 3.0)])),
            Some(snap(&[(1.0, 1.0), (2.0, 2.0), (3.0, 3.0)])),
        );
        assert_eq!(d.base_spans, Vec::<Range<usize>>::new());
        assert_eq!(d.head_spans, vec![1..2]);
    }
}

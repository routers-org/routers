//! A static R-tree over items that are already in spatial order.
//!
//! A graph's nodes are numbered along a Hilbert curve and its edges follow
//! their source nodes, so consecutive items are neighbours on the map. The
//! tree is therefore built without sorting: each box of a level bounds the
//! next [`FANOUT`] of the level below, in one pass.
//!
//! Boxes are held a column per bound, in single precision rounded outward:
//! testing a run of them is four comparisons down four short arrays. A box
//! only ever says an item may match; the item's own envelope decides.

use alloc::collections::BinaryHeap;
use core::cmp::Ordering;
use core::ops::Range;

use geo::{Point, Rect};

/// Boxes bounded by one box of the level above.
const FANOUT: usize = 16;

/// The boxes of one level, a column per bound.
#[derive(Debug, Default)]
pub(super) struct Level {
    min_x: Vec<f32>,
    min_y: Vec<f32>,
    max_x: Vec<f32>,
    max_y: Vec<f32>,
}

/// The largest `f32` not above `value`.
#[inline]
fn floor(value: f64) -> f32 {
    let near = value as f32;
    if f64::from(near) > value {
        near.next_down()
    } else {
        near
    }
}

/// The smallest `f32` not below `value`.
#[inline]
fn ceil(value: f64) -> f32 {
    let near = value as f32;
    if f64::from(near) < value {
        near.next_up()
    } else {
        near
    }
}

impl Level {
    fn with_capacity(boxes: usize) -> Self {
        Level {
            min_x: Vec::with_capacity(boxes),
            min_y: Vec::with_capacity(boxes),
            max_x: Vec::with_capacity(boxes),
            max_y: Vec::with_capacity(boxes),
        }
    }

    /// A box around each of `points`.
    pub(super) fn around(points: impl Iterator<Item = Point> + Clone) -> Self {
        Level {
            min_x: points.clone().map(|at| floor(at.x())).collect(),
            min_y: points.clone().map(|at| floor(at.y())).collect(),
            max_x: points.clone().map(|at| ceil(at.x())).collect(),
            max_y: points.map(|at| ceil(at.y())).collect(),
        }
    }

    /// A box around each pair of this level's boxes.
    pub(super) fn spanning(&self, pairs: impl Iterator<Item = (usize, usize)> + Clone) -> Self {
        let low = |column: &[f32]| {
            pairs
                .clone()
                .map(|(a, b)| column[a].min(column[b]))
                .collect()
        };
        let high = |column: &[f32]| {
            pairs
                .clone()
                .map(|(a, b)| column[a].max(column[b]))
                .collect()
        };

        Level {
            min_x: low(&self.min_x),
            min_y: low(&self.min_y),
            max_x: high(&self.max_x),
            max_y: high(&self.max_y),
        }
    }

    fn len(&self) -> usize {
        self.min_x.len()
    }

    fn push(&mut self, [min_x, min_y, max_x, max_y]: [f32; 4]) {
        self.min_x.push(min_x);
        self.min_y.push(min_y);
        self.max_x.push(max_x);
        self.max_y.push(max_y);
    }

    /// The box bounding `run` of this level's. A bound that is not a number
    /// is ignored.
    fn union(&self, run: Range<usize>) -> [f32; 4] {
        let low = |column: &[f32]| {
            column[run.clone()]
                .iter()
                .copied()
                .fold(f32::INFINITY, f32::min)
        };
        let high = |column: &[f32]| {
            column[run.clone()]
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max)
        };

        [
            low(&self.min_x),
            low(&self.min_y),
            high(&self.max_x),
            high(&self.max_y),
        ]
    }

    /// Which of `run` meet `bounds`, as a bit each.
    #[inline]
    fn meeting(&self, run: Range<usize>, bounds: &[f32; 4]) -> u32 {
        let [min_x, min_y, max_x, max_y] = *bounds;
        let (low_x, low_y) = (&self.min_x[run.clone()], &self.min_y[run.clone()]);
        let (high_x, high_y) = (&self.max_x[run.clone()], &self.max_y[run]);

        let mut met = 0;
        for at in 0..low_x.len() {
            let meets = (low_x[at] <= max_x)
                & (high_x[at] >= min_x)
                & (low_y[at] <= max_y)
                & (high_y[at] >= min_y);

            met |= u32::from(meets) << at;
        }
        met
    }

    /// Squared planar distance from `point` to the nearest of box `at`.
    #[inline]
    fn distance(&self, at: usize, point: &Point) -> f64 {
        let (min_x, max_x) = (f64::from(self.min_x[at]), f64::from(self.max_x[at]));
        let (min_y, max_y) = (f64::from(self.min_y[at]), f64::from(self.max_y[at]));

        let dx = (min_x - point.x()).max(point.x() - max_x).max(0.0);
        let dy = (min_y - point.y()).max(point.y() - max_y).max(0.0);

        dx * dx + dy * dy
    }
}

#[inline]
fn meets((min, max): (Point, Point), bounds: &Rect<f64>) -> bool {
    min.x() <= bounds.max().x
        && max.x() >= bounds.min().x
        && min.y() <= bounds.max().y
        && max.y() >= bounds.min().y
}

/// Something to look at next, nearest first.
struct Near {
    distance: f64,
    /// The level a box is of; `None` for an item itself.
    level: Option<usize>,
    at: usize,
}

impl PartialEq for Near {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Near {}

impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Near {
    fn cmp(&self, other: &Self) -> Ordering {
        other.distance.total_cmp(&self.distance)
    }
}

#[derive(Debug, Default)]
pub(super) struct Packed {
    /// `levels[0]` holds a box per item; each later level bounds runs of the
    /// one before. The last holds at most [`FANOUT`] boxes.
    levels: Vec<Level>,
}

impl Packed {
    /// Index items, in order, by a box around each.
    pub(super) fn over(mut level: Level) -> Self {
        let mut levels = Vec::new();
        while level.len() > FANOUT {
            let mut above = Level::with_capacity(level.len().div_ceil(FANOUT));
            for start in (0..level.len()).step_by(FANOUT) {
                above.push(level.union(start..(start + FANOUT).min(level.len())));
            }

            levels.push(core::mem::replace(&mut level, above));
        }
        levels.push(level);

        Packed { levels }
    }

    pub(super) fn len(&self) -> usize {
        self.levels.first().map_or(0, Level::len)
    }

    /// The boxes of `level` that box `at` of the level above bounds.
    fn below(&self, level: usize, at: usize) -> Range<usize> {
        at * FANOUT..((at + 1) * FANOUT).min(self.levels[level].len())
    }

    /// Every item whose envelope meets `bounds`, in item order: a query's
    /// answer does not depend on how the tree happens to be shaped.
    pub(super) fn search(
        &self,
        bounds: Rect<f64>,
        envelope: impl Fn(usize) -> (Point, Point),
    ) -> Vec<u32> {
        let mut found = Vec::new();
        let Some(top) = self.levels.len().checked_sub(1) else {
            return found;
        };

        let (min, max) = (bounds.min(), bounds.max());
        let wide = [floor(min.x), floor(min.y), ceil(max.x), ceil(max.y)];

        // Runs of boxes yet to be tested, each with its level.
        let mut pending = vec![(top, 0..self.levels[top].len())];

        while let Some((level, run)) = pending.pop() {
            let mut met = self.levels[level].meeting(run.clone(), &wide);

            while met != 0 {
                let at = run.start + met.trailing_zeros() as usize;
                met &= met - 1;

                match level {
                    0 if meets(envelope(at), &bounds) => found.push(at as u32),
                    0 => {}
                    level => pending.push((level - 1, self.below(level - 1, at))),
                }
            }
        }

        found.sort_unstable();
        found
    }

    /// The item nearest `point`, by planar distance over coordinates.
    pub(super) fn nearest(&self, point: &Point, position: impl Fn(usize) -> Point) -> Option<u32> {
        let top = self.levels.len().checked_sub(1)?;

        let boxed = |level: usize, at: usize| Near {
            distance: self.levels[level].distance(at, point),
            level: Some(level),
            at,
        };
        let mut nearest = (0..self.levels[top].len())
            .map(|at| boxed(top, at))
            .collect::<BinaryHeap<_>>();

        while let Some(Near { level, at, .. }) = nearest.pop() {
            match level {
                None => return Some(at as u32),
                Some(0) => {
                    let position = position(at);
                    let (dx, dy) = (position.x() - point.x(), position.y() - point.y());
                    let distance = dx * dx + dy * dy;

                    // An item with no position is nowhere near.
                    if !distance.is_nan() {
                        nearest.push(Near {
                            distance,
                            level: None,
                            at,
                        });
                    }
                }
                Some(level) => {
                    nearest.extend(self.below(level - 1, at).map(|at| boxed(level - 1, at)))
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points along a curve that wanders but stays local, as a space-filling curve does.
    fn points(count: usize) -> Vec<Point> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };

        (0..count)
            .map(|at| Point::new(at as f64 * 0.01 + next(), next() * 3.0))
            .collect()
    }

    #[test]
    fn a_search_finds_exactly_what_a_scan_does() {
        for count in [0, 1, 15, 16, 17, 255, 256, 257, 5_000] {
            let points = points(count);
            let index = Packed::over(Level::around(points.iter().copied()));
            assert_eq!(index.len(), count);

            for (x, y, size) in [
                (0.0, 0.0, 0.5),
                (3.0, 1.0, 2.0),
                (25.0, 1.5, 0.1),
                (-5.0, -5.0, 1.0),
            ] {
                let bounds = Rect::new((x, y), (x + size, y + size));

                let found = index.search(bounds, |at| (points[at], points[at]));
                let scanned = (0..count as u32)
                    .filter(|&at| meets((points[at as usize], points[at as usize]), &bounds))
                    .collect::<Vec<_>>();

                assert_eq!(found, scanned, "{count} points, box at ({x}, {y})");
            }
        }
    }

    #[test]
    fn the_nearest_is_the_nearest() {
        for count in [1, 16, 17, 300, 5_000] {
            let points = points(count);
            let index = Packed::over(Level::around(points.iter().copied()));

            for probe in [
                Point::new(0.0, 0.0),
                Point::new(7.3, 1.1),
                Point::new(90.0, -4.0),
            ] {
                let found = index.nearest(&probe, |at| points[at]).expect("an item");
                let away = |at: usize| {
                    let (dx, dy) = (points[at].x() - probe.x(), points[at].y() - probe.y());
                    dx * dx + dy * dy
                };
                let closest = (0..count).map(away).fold(f64::INFINITY, f64::min);

                assert_eq!(away(found as usize), closest, "{count} points");
            }
        }

        assert_eq!(
            Packed::default().nearest(&Point::new(0.0, 0.0), |_| unreachable!()),
            None
        );
    }
}

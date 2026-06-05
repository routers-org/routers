//! Quad-tree sharding strategy.
//!
//! The world is treated as the rectangle `[-180, 180] x [-90, 90]`
//! and recursively subdivided into four equal quadrants.
//!
//! A [`QuadKey`] encodes the path from the root to a given
//! cell as 2 bits per level, packed LSB-first into a `u64`.

use core::fmt::{self, Debug};
use geo::{Point, Rect, coord};
use serde::{Deserialize, Serialize};
use std::fmt::Display;

const ROOT_MIN_X: f64 = -180.0;
const ROOT_MAX_X: f64 = 180.0;
const ROOT_MIN_Y: f64 = -90.0;
const ROOT_MAX_Y: f64 = 90.0;

const MAX_DEPTH: u8 = 31;

/// A path from the root of a quad-tree to a single cell.
///
/// Stored as `depth` (number of subdivision steps) plus `bits`, where the
/// 2 bits at position `2*i` describe which child was taken at level `i`:
///
/// - `0b00` -> south-west
/// - `0b01` -> south-east
/// - `0b10` -> north-west
/// - `0b11` -> north-east
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QuadKey {
    pub depth: u8,
    pub bits: u64,
}

impl Display for QuadKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QuadKey(d{}|", self.depth)?;
        for i in 0..self.depth {
            write!(f, "{}", self.child_at(i))?;
        }
        write!(f, ")")
    }
}

impl QuadKey {
    pub const fn root() -> Self {
        Self { depth: 0, bits: 0 }
    }

    /// Decode the quadrant taken at level `level` (0-indexed from the root).
    #[inline]
    fn child_at(&self, level: u8) -> u8 {
        ((self.bits >> (2 * level)) & 0b11) as u8
    }
}

impl Debug for QuadKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QuadKey(d{}|", self.depth)?;
        for i in 0..self.depth {
            write!(f, "{}", self.child_at(i))?;
        }
        write!(f, ")")
    }
}

/// Quad-tree partitioning of the WGS-84 world rectangle.
#[derive(Debug, Clone)]
pub struct QuadTreeStrategy {
    depth: u8,
}

impl QuadTreeStrategy {
    pub fn with_depth(depth: u8) -> Self {
        assert!(depth <= MAX_DEPTH, "quad-tree depth must be ≤ {MAX_DEPTH}");
        Self { depth }
    }

    pub const fn depth(&self) -> u8 {
        self.depth
    }
}

impl super::ShardingStrategy for QuadTreeStrategy {
    type Id = QuadKey;

    fn locate(&self, point: Point) -> QuadKey {
        let (mut min_x, mut max_x) = (ROOT_MIN_X, ROOT_MAX_X);
        let (mut min_y, mut max_y) = (ROOT_MIN_Y, ROOT_MAX_Y);
        let (px, py) = point.x_y();

        // Clamp into the root rectangle so that out-of-range points still get
        // a deterministic shard (the edge cells).
        let px = px.clamp(ROOT_MIN_X, ROOT_MAX_X);
        let py = py.clamp(ROOT_MIN_Y, ROOT_MAX_Y);

        let mut bits: u64 = 0;
        for level in 0..self.depth {
            let mid_x = 0.5 * (min_x + max_x);
            let mid_y = 0.5 * (min_y + max_y);

            let east = px >= mid_x;
            let north = py >= mid_y;
            let q: u64 = (north as u64) << 1 | (east as u64);
            bits |= q << (2 * level);

            if east {
                min_x = mid_x;
            } else {
                max_x = mid_x;
            }
            if north {
                min_y = mid_y;
            } else {
                max_y = mid_y;
            }
        }

        QuadKey {
            depth: self.depth,
            bits,
        }
    }

    fn bounds(&self, id: &QuadKey) -> Rect {
        let (mut min_x, mut max_x) = (ROOT_MIN_X, ROOT_MAX_X);
        let (mut min_y, mut max_y) = (ROOT_MIN_Y, ROOT_MAX_Y);

        for level in 0..id.depth {
            let mid_x = 0.5 * (min_x + max_x);
            let mid_y = 0.5 * (min_y + max_y);
            let q = id.child_at(level);
            if q & 0b01 != 0 {
                min_x = mid_x;
            } else {
                max_x = mid_x;
            }
            if q & 0b10 != 0 {
                min_y = mid_y;
            } else {
                max_y = mid_y;
            }
        }

        Rect::new(coord! { x: min_x, y: min_y }, coord! { x: max_x, y: max_y })
    }

    fn neighbours(&self, id: &QuadKey) -> Vec<QuadKey> {
        // Probe the eight cardinal neighbours by sampling a point just past
        // each edge of the cell. This works uniformly for any cell of the
        // same depth and avoids hand-rolling the (hairy) Z-order arithmetic.
        let rect = self.bounds(id);

        let (min_x, max_x) = (rect.min().x, rect.max().x);
        let (min_y, max_y) = (rect.min().y, rect.max().y);

        let w = max_x - min_x;
        let h = max_y - min_y;

        let eps_x = w * 0.25;
        let eps_y = h * 0.25;

        let cx = 0.5 * (min_x + max_x);
        let cy = 0.5 * (min_y + max_y);

        let probes = [
            (cx, max_y + eps_y),            // N
            (max_x + eps_x, max_y + eps_y), // NE
            (max_x + eps_x, cy),            // E
            (max_x + eps_x, min_y - eps_y), // SE
            (cx, min_y - eps_y),            // S
            (min_x - eps_x, min_y - eps_y), // SW
            (min_x - eps_x, cy),            // W
            (min_x - eps_x, max_y + eps_y), // NW
        ];

        let mut out = Vec::with_capacity(8);
        for (x, y) in probes {
            // Skip neighbours that fall outside the world rectangle: those
            // sit beyond the poles or wrap around the dateline, neither of
            // which the strategy promises to handle.
            if x < ROOT_MIN_X || x > ROOT_MAX_X || y < ROOT_MIN_Y || y > ROOT_MAX_Y {
                continue;
            }
            let n = self.locate(Point::new(x, y));
            if n != *id && !out.contains(&n) {
                out.push(n);
            }
        }

        out
    }
}

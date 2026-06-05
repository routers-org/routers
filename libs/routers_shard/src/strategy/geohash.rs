//! Geohash sharding strategy.
//!
//! Implements the standard base-32 geohash encoding. Provided primarily to
//! demonstrate that the [`ShardingStrategy`](super::ShardingStrategy) trait
//! is independent of any one partitioning scheme — swap [`QuadTreeStrategy`]
//! for [`GeohashStrategy`] without touching the ingestion path.

use core::fmt;
use geo::{Point, Rect, coord};
use serde::{Deserialize, Serialize};
use std::fmt::Display;

const BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";
const MAX_PRECISION: usize = 12;

/// A geohash, stored as its canonical base-32 string representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Geohash {
    data: [u8; MAX_PRECISION],
    pub precision: u8,
}

impl Display for Geohash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &c in &self.data[..self.precision as usize] {
            write!(f, "{}", BASE32[c as usize] as char)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct GeohashStrategy {
    precision: u8,
}

impl GeohashStrategy {
    pub fn with_precision(precision: u8) -> Self {
        assert!(
            precision >= 1 && precision <= 12,
            "geohash precision must be in 1..=12"
        );
        Self { precision }
    }

    pub const fn precision(&self) -> u8 {
        self.precision
    }
}


impl super::ShardingStrategy for GeohashStrategy {
    type Id = Geohash;

    fn locate(&self, point: Point) -> Geohash {
        let (mut min_x, mut max_x) = (-180.0f64, 180.0f64);
        let (mut min_y, mut max_y) = (-90.0f64, 90.0f64);

        let (px, py) = point.x_y();
        let px = px.clamp(min_x, max_x);
        let py = py.clamp(min_y, max_y);

        let mut out = [0u8; MAX_PRECISION];
        let mut cursor = 0;

        let mut bit = 0u8;
        let mut ch: u8 = 0;
        let mut even = true;

        while cursor < self.precision as usize {
            if even {
                let mid = 0.5 * (min_x + max_x);
                if px >= mid {
                    ch |= 1 << (4 - bit);
                    min_x = mid;
                } else {
                    max_x = mid;
                }
            } else {
                let mid = 0.5 * (min_y + max_y);
                if py >= mid {
                    ch |= 1 << (4 - bit);
                    min_y = mid;
                } else {
                    max_y = mid;
                }
            }
            even = !even;
            if bit < 4 {
                bit += 1;
            } else {
                out[cursor] = ch;
                cursor += 1;

                bit = 0;
                ch = 0;
            }
        }

        Geohash {
            data: out,
            precision: cursor as u8,
        }
    }

    fn bounds(&self, id: &Geohash) -> Rect {
        let (mut min_x, mut max_x) = (-180.0f64, 180.0f64);
        let (mut min_y, mut max_y) = (-90.0f64, 90.0f64);
        let mut even = true;

        for &idx in &id.data[..id.precision as usize] {
            for i in (0..5).rev() {
                let bit = (idx >> i) & 1;
                if even {
                    let mid = 0.5 * (min_x + max_x);
                    if bit == 1 {
                        min_x = mid;
                    } else {
                        max_x = mid;
                    }
                } else {
                    let mid = 0.5 * (min_y + max_y);
                    if bit == 1 {
                        min_y = mid;
                    } else {
                        max_y = mid;
                    }
                }
                even = !even;
            }
        }
        Rect::new(coord! { x: min_x, y: min_y }, coord! { x: max_x, y: max_y })
    }

    fn neighbours(&self, id: &Geohash) -> Vec<Self::Id> {
        // Same edge-probe technique as the quad-tree: sample a point just
        // past each of the 8 cardinal edges and re-encode. Avoids re-doing
        // the geohash adjacency table.
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
            (cx, max_y + eps_y),
            (max_x + eps_x, max_y + eps_y),
            (max_x + eps_x, cy),
            (max_x + eps_x, min_y - eps_y),
            (cx, min_y - eps_y),
            (min_x - eps_x, min_y - eps_y),
            (min_x - eps_x, cy),
            (min_x - eps_x, max_y + eps_y),
        ];

        let mut out = Vec::with_capacity(8);
        for (x, y) in probes {
            if x < -180.0 || x > 180.0 || y < -90.0 || y > 90.0 {
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

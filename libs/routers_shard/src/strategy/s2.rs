//! S2 sharding strategy.
//!
//! Partitions the sphere with the S2 cell hierarchy: the unit sphere is
//! projected onto the six faces of a cube, each face is recursively split
//! into four, and the cells of one level are numbered along a Hilbert curve.
//!
//! Compared with [`QuadTreeStrategy`](super::quadtree::QuadTreeStrategy) and
//! [`GeohashStrategy`](super::geohash::GeohashStrategy), which subdivide the
//! equirectangular `[-180, 180] x [-90, 90]` rectangle, S2 cells:
//!
//! - have near-uniform area at a given level, with no degeneracy at the
//!   poles;
//! - wrap across the antimeridian, so every cell has a complete
//!   neighbourhood: 8 neighbours, or 7 for the cells that touch one of the
//!   eight cube vertices;
//! - are *not* rectangles in latitude/longitude space. Because of this,
//!   [`bounds`](super::ShardingStrategy::bounds) returns a conservative
//!   bounding rectangle, and [`contains`](super::ShardingStrategy::contains)
//!   tests exact cell membership instead of the rectangle.
//!
//! The backing implementation is the [`s2`] crate; this module adapts its
//! `CellID` to the [`ShardId`](super::ShardId) contract. Requires the `s2`
//! feature.

use ::s2::cell::Cell;
use ::s2::cellid::{CellID, MAX_LEVEL};
use ::s2::latlng::LatLng;
use core::fmt::{self, Debug, Display};
use core::str::FromStr;
use geo::{Point, Rect, coord};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The deepest S2 subdivision level (leaf cells, roughly 1 cm across).
pub const MAX_S2_LEVEL: u8 = MAX_LEVEL as u8;

/// A single S2 cell, stored as its 64-bit cell id.
///
/// The level is encoded in the id (by the position of its lowest set bit),
/// so ids from strategies of different levels never collide. Ordering
/// follows the Hilbert curve, which keeps geographically close cells close
/// in sort order.
///
/// [`Display`] renders the canonical S2 *token*: the id in hexadecimal with
/// trailing zeros stripped (`"1"` for face 0, `"47a1cbd595522b39"` for a
/// leaf). [`FromStr`] parses the same form.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct S2CellId(u64);

impl S2CellId {
    /// Wrap a raw 64-bit S2 cell id, or `None` if it is not a valid cell.
    pub fn from_raw(raw: u64) -> Option<Self> {
        CellID(raw).is_valid().then_some(Self(raw))
    }

    /// The raw 64-bit S2 cell id.
    pub const fn raw(&self) -> u64 {
        self.0
    }

    /// The subdivision level of this cell (`0` for a cube face, `30` for a
    /// leaf).
    pub fn level(&self) -> u8 {
        self.cell_id().level() as u8
    }

    /// The cube face (`0..6`) that this cell lies on.
    pub fn face(&self) -> u8 {
        self.cell_id().face()
    }

    /// The canonical S2 token for this cell.
    pub fn token(&self) -> String {
        self.cell_id().to_token()
    }

    #[inline]
    const fn cell_id(&self) -> CellID {
        CellID(self.0)
    }
}

impl Display for S2CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.token())
    }
}

impl Debug for S2CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "S2CellId(l{}|{})", self.level(), self.token())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum S2ParseError {
    #[error("invalid S2 cell token '{0}'")]
    InvalidToken(String),
}

impl FromStr for S2CellId {
    type Err = S2ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // `CellID::from_token` is lenient (it accepts a leading sign and
        // maps garbage to the invalid id 0), so validate the shape first.
        let is_hex = s.bytes().all(|b| b.is_ascii_hexdigit());
        if s.is_empty() || s.len() > 16 || !is_hex {
            return Err(S2ParseError::InvalidToken(s.to_owned()));
        }
        Self::from_raw(CellID::from_token(s).0)
            .ok_or_else(|| S2ParseError::InvalidToken(s.to_owned()))
    }
}

/// S2 cell partitioning of the sphere at a fixed level.
#[derive(Debug, Clone)]
pub struct S2Strategy {
    level: u8,
}

impl S2Strategy {
    pub fn with_level(level: u8) -> Self {
        assert!(level <= MAX_S2_LEVEL, "S2 level must be ≤ {MAX_S2_LEVEL}");
        Self { level }
    }

    pub const fn level(&self) -> u8 {
        self.level
    }

    /// The leaf cell containing `point`.
    ///
    /// Latitude is clamped so that out-of-range points still land in a
    /// deterministic cell. Longitude is periodic on the sphere, so it needs
    /// no clamp: `190°` is the same meridian as `-170°`.
    #[inline]
    fn leaf(point: Point) -> CellID {
        let (lng, lat) = point.x_y();
        CellID::from(LatLng::from_degrees(lat.clamp(-90.0, 90.0), lng))
    }
}

impl super::ShardingStrategy for S2Strategy {
    type Id = S2CellId;

    fn locate(&self, point: Point) -> S2CellId {
        S2CellId(Self::leaf(point).parent(u64::from(self.level)).0)
    }

    /// A conservative latitude/longitude bounding rectangle of the cell.
    ///
    /// S2 cells are bounded by great-circle arcs, so the rectangle is a
    /// superset of the cell. Cells that touch a pole span the full longitude
    /// range, as do the six level-0 face cells whose bound wraps.
    fn bounds(&self, id: &S2CellId) -> Rect {
        let cell_id = id.cell_id();
        let bound = Cell::from(cell_id).rect_bound();

        let min_y = bound.lat.lo.to_degrees().clamp(-90.0, 90.0);
        let max_y = bound.lat.hi.to_degrees().clamp(-90.0, 90.0);

        let lo_x = bound.lng.lo.to_degrees().clamp(-180.0, 180.0);
        let hi_x = bound.lng.hi.to_degrees().clamp(-180.0, 180.0);

        let (min_x, max_x) = if !bound.lng.is_inverted() {
            (lo_x, hi_x)
        } else if cell_id.is_face() {
            // Face 3 is centred on the antimeridian, so its longitude bound
            // genuinely wraps and only the full span is representable.
            (-180.0, 180.0)
        } else {
            // At level >= 1 every cell edge that meets the antimeridian runs
            // along it (it is the `v = 0` line of faces 2, 3 and 5), so no
            // cell straddles it. An inverted interval here is `rect_bound`'s
            // epsilon expansion pushing an endpoint past ±180°; snap it back
            // to the side the cell is actually on.
            if LatLng::from(cell_id).lng.deg() >= 0.0 {
                (lo_x, 180.0)
            } else {
                (-180.0, hi_x)
            }
        };

        Rect::new(coord! { x: min_x, y: min_y }, coord! { x: max_x, y: max_y })
    }

    /// All cells at the same level whose boundary touches `id`'s boundary,
    /// including the diagonal ones that share only a vertex.
    ///
    /// Neighbours wrap across faces, the antimeridian and the poles, so the
    /// result has 8 entries, or 7 for a cell at one of the eight cube
    /// vertices where only three faces meet.
    fn neighbours(&self, id: &S2CellId) -> Vec<S2CellId> {
        let cell_id = id.cell_id();
        let mut out = Vec::with_capacity(8);
        // `all_neighbors` may report a neighbour twice around a face vertex.
        for n in cell_id.all_neighbors(cell_id.level()) {
            let n = S2CellId(n.0);
            if n != *id && !out.contains(&n) {
                out.push(n);
            }
        }
        out
    }

    /// Exact membership test: the cell is not a latitude/longitude
    /// rectangle, so the default rectangle test would admit points beyond
    /// its curved edges.
    #[inline]
    fn contains(&self, id: &S2CellId, point: Point) -> bool {
        id.cell_id().contains(&Self::leaf(point))
    }
}

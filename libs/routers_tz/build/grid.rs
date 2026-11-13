use geo::{Coord, Rect};

use crate::partition::{Bounds, Region};

const ROOT_DEGREES: f64 = 45.0;
pub const MAX_DEPTH: u8 = 12;

// Centres are dyadic, as are many borders (e.g. 67.5°W in Antarctica), so the reference point sits off-centre.
const REFERENCE: Coord = Coord {
    x: 0.5 + 0.013_572_4,
    y: 0.5 - 0.011_838_6,
};

#[derive(Clone, Copy, PartialEq)]
pub struct GridBox {
    pub rect: Rect,
    depth: u8,
}

pub fn roots() -> Vec<GridBox> {
    (0..8)
        .flat_map(|col| (0..4).map(move |row| (col, row)))
        .map(|(col, row)| {
            let min = Coord {
                x: -180.0 + col as f64 * ROOT_DEGREES,
                y: -90.0 + row as f64 * ROOT_DEGREES,
            };
            let size = Coord {
                x: ROOT_DEGREES,
                y: ROOT_DEGREES,
            };
            GridBox {
                rect: Rect::new(min, min + size),
                depth: 0,
            }
        })
        .collect()
}

impl Region for GridBox {
    fn bounds(&self) -> Bounds {
        let (min, max) = (self.rect.min(), self.rect.max());
        Bounds {
            lat: (min.y, max.y),
            lng: [(min.x, max.x); 2],
        }
    }

    fn centre(&self) -> Coord {
        let (min, max) = (self.rect.min(), self.rect.max());
        Coord {
            x: min.x + (max.x - min.x) * REFERENCE.x,
            y: min.y + (max.y - min.y) * REFERENCE.y,
        }
    }

    fn is_finest(&self) -> bool {
        self.depth == MAX_DEPTH
    }

    fn children(&self) -> [Self; 4] {
        let (min, max, mid) = (self.rect.min(), self.rect.max(), self.rect.center());
        let child = |a: Coord, b: Coord| GridBox {
            rect: Rect::new(a, b),
            depth: self.depth + 1,
        };
        [
            child(min, mid),
            child(Coord { x: mid.x, y: min.y }, Coord { x: max.x, y: mid.y }),
            child(Coord { x: min.x, y: mid.y }, Coord { x: mid.x, y: max.y }),
            child(mid, max),
        ]
    }
}

/// Joins boxes with the same index that share a whole side, first along x and then along y.
pub fn merge(boxes: Vec<(Rect, u16)>) -> Vec<(Rect, u16)> {
    merge_along(
        merge_along(boxes, |r| (r.min().x, r.max().x, r.min().y, r.max().y)),
        |r| (r.min().y, r.max().y, r.min().x, r.max().x),
    )
}

/// `span` gives (start, end) along the axis, then (start, end) across it.
fn merge_along(
    mut boxes: Vec<(Rect, u16)>,
    span: impl Fn(&Rect) -> (f64, f64, f64, f64),
) -> Vec<(Rect, u16)> {
    boxes.sort_unstable_by(|(a, _), (b, _)| {
        let (a, b) = (span(a), span(b));
        (a.2, a.3, a.0)
            .partial_cmp(&(b.2, b.3, b.0))
            .expect("box bounds are finite")
    });

    let mut merged: Vec<(Rect, u16)> = Vec::with_capacity(boxes.len());
    for (rect, index) in boxes {
        if let Some((last, last_index)) = merged.last_mut() {
            let (prev, next) = (span(last), span(&rect));
            if *last_index == index && (prev.2, prev.3) == (next.2, next.3) && prev.1 == next.0 {
                *last = Rect::new(last.min(), rect.max());
                continue;
            }
        }
        merged.push((rect, index));
    }
    merged
}

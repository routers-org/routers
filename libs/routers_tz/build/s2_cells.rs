use geo::Coord;
use s2::cell::Cell;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use crate::partition::{Bounds, Region};

pub const MAX_LEVEL: u64 = 13;

// Face centres sit on the poles, which lie on polygon edges.
const ROOT_LEVEL: u64 = 2;

pub fn roots() -> Vec<CellID> {
    (0..6)
        .map(CellID::from_face)
        .flat_map(|face| {
            let end = face.child_end_at_level(ROOT_LEVEL);
            core::iter::successors(Some(face.child_begin_at_level(ROOT_LEVEL)), |id| {
                Some(id.next())
            })
            .take_while(move |id| *id != end)
        })
        .collect()
}

impl Region for CellID {
    fn bounds(&self) -> Bounds {
        let rect = Cell::from(self).rect_bound();
        let (lo, hi) = (rect.lng.lo.to_degrees(), rect.lng.hi.to_degrees());
        let lng = if rect.lng.is_full() {
            [(-180.0, 180.0); 2]
        } else if rect.lng.is_inverted() {
            [(lo, 180.0), (-180.0, hi)]
        } else {
            [(lo, hi); 2]
        };

        Bounds {
            lat: (rect.lat.lo.to_degrees(), rect.lat.hi.to_degrees()),
            lng,
        }
    }

    fn centre(&self) -> Coord {
        let ll = LatLng::from(&Cell::from(self).center());
        Coord {
            x: ll.lng.deg(),
            y: ll.lat.deg(),
        }
    }

    fn is_finest(&self) -> bool {
        self.level() == MAX_LEVEL
    }

    fn children(&self) -> [Self; 4] {
        CellID::children(self)
    }
}

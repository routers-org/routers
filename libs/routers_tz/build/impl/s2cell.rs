use geo::Coord;
use routers_tz_types::storage::s2cell::{CELL_SHIFT, S2StorageBackend};
use routers_tz_types::timezone::internal::TimezoneBuild;
use s2::cell::Cell;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use crate::BoxError;
use crate::codegen::Backend;
use crate::partition::{Bounds, Region, partition};

const MAX_LEVEL: u64 = 13;

/// Descent starts here: face centres sit on the poles, which lie on polygon edges.
const ROOT_LEVEL: u64 = 2;

// A CellID is 3 face bits, 2 bits per level, then the sentinel; shifting must not drop any.
const _: () = assert!(CELL_SHIFT == 60 - 2 * MAX_LEVEL as u32);

pub fn build(timezones: &[TimezoneBuild]) -> Result<(), BoxError> {
    let backend = Backend { module: "s2cell" };
    if backend.is_fresh() {
        return Ok(());
    }

    backend.emit(compute_backend(timezones))
}

fn compute_backend(timezones: &[TimezoneBuild]) -> S2StorageBackend {
    let roots: Vec<CellID> = (0..6)
        .map(CellID::from_face)
        .flat_map(|face| {
            let end = face.child_end_at_level(ROOT_LEVEL);
            core::iter::successors(Some(face.child_begin_at_level(ROOT_LEVEL)), |id| Some(id.next()))
                .take_while(move |id| *id != end)
        })
        .collect();

    let mut cells = partition(roots, timezones);
    cells.sort_unstable_by_key(|(id, _)| *id);
    eprintln!("[s2cell] {} cells", cells.len());

    let (cell_ids, tz_indices) = cells
        .into_iter()
        .map(|(id, zone)| ((id.0 >> CELL_SHIFT) as u32, zone))
        .unzip();

    S2StorageBackend {
        cell_ids,
        tz_indices,
        names: timezones.iter().map(|tz| tz.name.clone()).collect(),
    }
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

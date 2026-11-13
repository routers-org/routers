use routers_tz_types::storage::s2cell::{CELL_SHIFT, S2StorageBackend};
use routers_tz_types::timezone::internal::TimezoneBuild;

use crate::BoxError;
use crate::codegen::Backend;
use crate::partition::{ZoneSets, partition};
use crate::s2_cells::{self, MAX_LEVEL};

// A CellID is 3 face bits, 2 bits per level, then the sentinel; shifting must not drop any.
const _: () = assert!(CELL_SHIFT == 60 - 2 * MAX_LEVEL as u32);

pub fn build(timezones: &[TimezoneBuild]) -> Result<(), BoxError> {
    let backend = Backend { module: "s2cell" };
    if backend.is_fresh() {
        return Ok(());
    }

    let mut cells = partition(s2_cells::roots(), timezones);
    cells.sort_unstable_by_key(|(id, _)| *id);
    eprintln!("[s2cell] {} cells", cells.len());

    let mut sets = ZoneSets::new(timezones.len());
    let (cell_ids, tz_indices) = cells
        .into_iter()
        .map(|(id, zones)| ((id.0 >> CELL_SHIFT) as u32, sets.index(zones)))
        .unzip();

    backend.emit(S2StorageBackend {
        cell_ids,
        tz_indices,
        names: timezones.iter().map(|tz| tz.name.clone()).collect(),
        overlaps: sets.overlaps,
    })
}

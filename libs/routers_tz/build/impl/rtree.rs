use routers_tz_types::storage::rtree::EncodableRTreeStorageBackend;
use routers_tz_types::timezone::internal::TimezoneBuild;

use crate::BoxError;
use crate::codegen::Backend;
use crate::grid;
use crate::partition::{ZoneSets, partition};

pub fn build(timezones: &[TimezoneBuild]) -> Result<(), BoxError> {
    let backend = Backend { module: "rtree" };
    if backend.is_fresh() {
        return Ok(());
    }

    let mut sets = ZoneSets::new(timezones.len());
    let boxes = (partition(grid::roots(), timezones).into_iter())
        .map(|(cell, zones)| (cell.rect, sets.index(zones)))
        .collect();
    let boxes = grid::merge(boxes);
    eprintln!("[rtree] {} boxes", boxes.len());

    let names = timezones.iter().map(|tz| tz.name.clone()).collect();
    backend.emit(EncodableRTreeStorageBackend::new(
        &boxes,
        names,
        sets.overlaps,
    ))
}

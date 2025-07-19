use geo::{Point, Polygon};
use geo_index::IndexableNum;
use geo_index::rtree::sort::HilbertSort;
use geo_index::rtree::{RTree, RTreeBuilder};
use std::collections::BTreeMap;

use crate::interface::{ResolvedTimezones, Timezone, TimezoneResolver};

const BOX_STORAGE: &'static [u8] = include_bytes!("boxes.bin");
const BOX_INDEX: &'static [u8] = include_bytes!("boxes.index.bin");

pub struct RTreeStorage {
    tree: RTree<u8>,
    index: BTreeMap<u32, Timezone>,
}

impl RTreeStorage {
    pub fn new() -> Self {
        let rtree = RTreeStorage::build_from_boxes(BOX_STORAGE);

        RTreeStorage {
            tree: rtree,
            index: todo!(),
        }
    }

    fn build_from_boxes<N: IndexableNum>(boxes_buf: &[N]) -> RTree<N> {
        let mut builder = RTreeBuilder::new((boxes_buf.len() / 4) as _);

        for box_ in boxes_buf.chunks(4) {
            let min_x = box_[0];
            let min_y = box_[1];
            let max_x = box_[2];
            let max_y = box_[3];
            builder.add(min_x, min_y, max_x, max_y);
        }

        builder.finish::<HilbertSort>()
    }
}

impl TimezoneResolver for RTreeStorage {
    type Error = ();

    fn area(&self, area: Polygon) -> Result<ResolvedTimezones, Self::Error> {
        todo!()
    }

    fn point(&self, point: Point) -> Result<ResolvedTimezones, Self::Error> {
        todo!()
    }
}

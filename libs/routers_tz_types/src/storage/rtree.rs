use crate::timezone::internal::TimeZoneName;
use geo::Rect;
use geo_index::rtree::sort::HilbertSort;
use geo_index::rtree::{RTreeBuilder, RTreeRef};
use ouroboros::self_referencing;
use serde::{Deserialize, Deserializer, Serialize};

#[self_referencing]
pub struct InternalTree {
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    tree: RTreeRef<'this, f32>,
}

impl InternalTree {
    // Cannot be type-defined using the `Borrow` trait due
    // to the covariant requirement with self's lifetime.
    pub fn borrow_this<'this, 'ext>(&'ext self) -> &'ext RTreeRef<'this, f32>
    where
        'ext: 'this,
    {
        self.borrow_tree()
    }
}

impl<'de> Deserialize<'de> for InternalTree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let data: Vec<u8> = Vec::deserialize(deserializer)?;
        Ok(InternalTreeBuilder {
            data,
            tree_builder: |data| RTreeRef::try_new(data).unwrap(),
        }
        .build())
    }
}

/// Disjoint lng/lat boxes; `zones[i]` holds the zones of tree item `i` (see [`super::zones`]).
#[derive(Deserialize)]
pub struct DecodableRTreeStorageBackend {
    pub tree: InternalTree,
    pub zones: Vec<u16>,
    pub names: Vec<TimeZoneName>,
    pub overlaps: Vec<Vec<u16>>,
}

impl DecodableRTreeStorageBackend {
    pub fn zones(&self, item: usize) -> &[u16] {
        super::zones(&self.zones[item], self.names.len(), &self.overlaps)
    }
}

// Alias.
pub type RTreeStorageBackend = DecodableRTreeStorageBackend;

#[derive(Serialize, Debug)]
pub struct EncodableRTreeStorageBackend {
    pub tree: Vec<u8>,
    pub zones: Vec<u16>,
    pub names: Vec<TimeZoneName>,
    pub overlaps: Vec<Vec<u16>>,
}

impl EncodableRTreeStorageBackend {
    /// Box corners are multiples of 45/4096°, which `f32` holds exactly.
    pub fn new(boxes: &[(Rect, u16)], names: Vec<TimeZoneName>, overlaps: Vec<Vec<u16>>) -> Self {
        let mut builder = RTreeBuilder::<f32>::new(boxes.len() as u32);
        let zones = boxes
            .iter()
            .map(|(rect, zone)| {
                let (min, max) = (rect.min(), rect.max());
                builder.add(min.x as f32, min.y as f32, max.x as f32, max.y as f32);
                *zone
            })
            .collect();

        EncodableRTreeStorageBackend {
            tree: builder.finish::<HilbertSort>().into_inner(),
            zones,
            names,
            overlaps,
        }
    }
}

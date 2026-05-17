use crate::timezone::internal::{TimeZoneName, TimezoneBuild};
use geo_index::rtree::sort::HilbertSort;
use geo_index::rtree::{RTreeBuilder, RTreeRef};
use ouroboros::self_referencing;
use serde::{Deserialize, Deserializer, Serialize};

#[self_referencing]
pub struct InternalTree {
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    tree: RTreeRef<'this, f64>,
}

impl InternalTree {
    // Cannot be type-defined using the `Borrow` trait due
    // to the covariant requirement with self's lifetime.
    pub fn borrow_this<'this, 'ext>(&'ext self) -> &'ext RTreeRef<'this, f64>
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

#[derive(Deserialize)]
pub struct DecodableRTreeStorageBackend {
    pub tree: InternalTree,
    pub names: Vec<TimeZoneName>,
}

// Alias.
pub type RTreeStorageBackend = DecodableRTreeStorageBackend;

#[derive(Serialize, Debug)]
pub struct EncodableRTreeStorageBackend {
    pub tree: Vec<u8>,
    pub names: Vec<TimeZoneName>,
}

impl EncodableRTreeStorageBackend {
    pub fn new(timezones: &[TimezoneBuild]) -> Self {
        Self::build_from_tzs(timezones).expect("failed to construct tree from timezones")
    }

    fn build_from_tzs(tzs: &[TimezoneBuild]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut builder = RTreeBuilder::new(tzs.len() as _);
        let mut names = Vec::new();

        for (i, TimezoneBuild { name, geometry, .. }) in tzs.into_iter().enumerate() {
            let bbox = geometry
                .bbox()
                .ok_or(format!("no bounding box for tz::[{i}]"))?;

            builder.add_rect(&bbox);
            names.push(name.clone());
        }

        let tree = builder.finish::<HilbertSort>();

        Ok(EncodableRTreeStorageBackend {
            tree: tree.into_inner(),
            names,
        })
    }
}

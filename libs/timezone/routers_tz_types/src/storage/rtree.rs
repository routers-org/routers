use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;
use bincode::{Decode, Encode};
use bincode::enc::write::Writer;

use geo_index::rtree::sort::HilbertSort;
use geo_index::rtree::{RTree, RTreeBuilder, RTreeRef};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::de::{Error, Visitor};
use crate::timezone::Timezone;

#[repr(transparent)]
#[derive(Debug, Clone)]
pub struct InternalTree {
    tree: RTreeRef<'static, f64>,
    data: &'static [u8]
}

impl Deref for InternalTree {
    type Target = RTreeRef<'static, f64>;

    fn deref(&self) -> &Self::Target {
        &self.tree
    }
}

impl Serialize for InternalTree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(&self.data)
    }
}

impl<'de> Deserialize<'de> for InternalTree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RTreeVisitor;

        impl<'de> Visitor<'de> for RTreeVisitor {
            type Value = InternalTree;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("internal rtree repr")
            }

            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
            where
                E: Error,
            {
                RTreeRef::try_new(v)
            }
        }

        deserializer.deserialize_bytes(RTreeVisitor)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RTreeStorageImpl {
    pub tree: InternalTree,
    pub index: BTreeMap<usize, Timezone>,
}

#[derive(Encode, Decode, Debug, Clone)]
pub struct RTreeStorageBackend {
    #[bincode(with_serde)]
    __impl: RTreeStorageImpl,
}

impl Deref for RTreeStorageBackend {
    type Target = RTreeStorageImpl;

    fn deref(&self) -> &Self::Target {
        &self.__impl
    }
}

impl RTreeStorageBackend {
    pub fn new(timezones: Vec<Timezone>) -> Self {
        RTreeStorageBackend::build_from_tzs(timezones)
            .expect("failed to construct tree from timezones")
    }

    fn build_from_tzs(tzs: Vec<Timezone>) -> Option<Self> {
        let mut builder = RTreeBuilder::new(tzs.len() as _);
        let mut index = BTreeMap::new();

        for (i, tz) in tzs.into_iter().enumerate() {
            let bbox = tz.bounding_box()?;

            builder.add_rect(&bbox);
            index.insert(i, tz)?;
        }

        let tree = builder.finish::<HilbertSort>();

        Some(RTreeStorageBackend {
            __impl: RTreeStorageImpl {
                tree: InternalTree {
                    t
                    data: tree.as_ref()
                },
                index
            }
        })
    }
}
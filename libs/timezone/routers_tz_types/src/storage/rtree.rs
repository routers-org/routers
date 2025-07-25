use crate::timezone::{IANATimezoneName, Timezone};
use bincode::{Decode, Encode};
use geo::{BoundingRect, ConvexHull, Geometry, MultiPolygon, Polygon};
use geo_index::rtree::sort::HilbertSort;
use geo_index::rtree::{RTreeBuilder, RTreeRef};
use geozero::ToWkt;
use ouroboros::self_referencing;
use serde::de::{Error, Visitor};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;

#[self_referencing]
pub struct InternalTree {
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    tree: RTreeRef<'this, f64>,
}

impl InternalTree {
    pub fn borrow<'this>(&'this self) -> &'this RTreeRef<'this, f64> {
        self.borrow_tree::<'this>()
    }
}

// impl Serialize for InternalTree {
//     fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
//     where
//         S: Serializer,
//     {
//         serializer.serialize_bytes(&self.data)
//     }
// }

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

            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
            where
                E: Error,
            {
                let tree = InternalTreeBuilder {
                    data: v,
                    tree_builder: |data| RTreeRef::try_new(data).unwrap(),
                }
                .build();

                Ok(tree)
            }
        }

        deserializer.deserialize_byte_buf(RTreeVisitor)
    }
}

#[derive(Decode)]
pub struct DecodableRTreeStorageBackend {
    #[bincode(with_serde)]
    pub tree: InternalTree,

    pub names: Vec<IANATimezoneName>,
    pub geometries: Vec<String>,
}

// Alias.
pub type RTreeStorageBackend = DecodableRTreeStorageBackend;

#[derive(Encode, Debug, Clone)]
pub struct EncodableRTreeStorageBackend {
    pub tree: Vec<u8>,

    pub names: Vec<IANATimezoneName>,
    pub geometries: Vec<String>,
}

impl EncodableRTreeStorageBackend {
    pub fn new(timezones: Vec<Timezone>) -> Self {
        Self::build_from_tzs(timezones).expect("failed to construct tree from timezones")
    }

    fn build_from_tzs(tzs: Vec<Timezone>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut builder = RTreeBuilder::new(tzs.len() as _);

        let mut names = Vec::new();
        let mut geometries = Vec::new();

        for (i, Timezone { iana, geometry }) in tzs.into_iter().enumerate() {
            let bbox = geometry
                .bounding_rect()
                .ok_or(format!("no bounding box for tz {}", i))?;

            builder.add_rect(&bbox);

            // todo; convex hull then simplify

            names.push(iana);
            geometries.push(
                Geometry::MultiPolygon(geometry)
                    .to_wkt()
                    .expect("failed to convert geometry to wkt"),
            );
        }

        let tree = builder.finish::<HilbertSort>();

        Ok(EncodableRTreeStorageBackend {
            tree: tree.into_inner(),

            names,
            geometries,
        })
    }
}

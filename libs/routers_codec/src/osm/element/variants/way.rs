//! Describes the minimal `Way` structure.
//! Has methods for accessing appropriate
//! tags for graph representation.

#[cfg(debug_assertions)]
use crate::osm::relation::MemberType;

use super::common::{OsmEntryId, ReferenceKey, References, Referential, Taggable, Tags};
use crate::osm;
use crate::osm::PrimitiveBlock;
use crate::osm::element::variants::Intermediate;

#[derive(Clone, Debug)]
pub struct Way {
    id: OsmEntryId,
    refs: References,
    tags: Tags,
}

impl Way {
    pub fn id(&self) -> OsmEntryId {
        self.id
    }

    #[inline]
    pub fn tags_owned(self) -> Tags {
        self.tags
    }

    #[inline]
    pub fn tags(&self) -> &Tags {
        &self.tags
    }

    #[inline]
    pub fn refs(&self) -> &References {
        &self.refs
    }

    #[inline]
    pub fn from_raw(value: &osm::Way, block: &PrimitiveBlock) -> Self {
        Way {
            id: OsmEntryId::way(value.id),
            refs: value.references(block),
            tags: value.tags(block),
        }
    }
}

impl Taggable for osm::Way {
    fn indices(&self) -> impl Iterator<Item = (&u32, &u32)> {
        self.keys.iter().zip(self.vals.iter())
    }
}

impl Referential for osm::Way {
    fn indices(&self) -> impl Iterator<Item = ReferenceKey<'_>> {
        self.refs.iter().map(|id| Intermediate {
            role: &-1i32,
            index: id,
            #[cfg(debug_assertions)]
            member_type: &(MemberType::Node as i32),
        })
    }
}

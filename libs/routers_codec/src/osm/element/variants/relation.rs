use super::common::{ReferenceKey, References, Referential, Taggable, Tags};
use crate::osm;
use crate::osm::element::variants::Intermediate;

#[derive(Clone, Debug)]
pub struct Relation<'a> {
    pub id: i64,
    pub tags: Tags<'a>,
    pub refs: References,
}

impl<'a> Relation<'a> {
    pub fn from_raw(relation: &'a osm::Relation, block: &'a osm::PrimitiveBlock) -> Self {
        Self {
            id: relation.id,
            tags: relation.tags(block),
            refs: relation.references(block),
        }
    }
}

impl Taggable for osm::Relation {
    fn indices(&self) -> impl Iterator<Item = (&u32, &u32)> {
        self.keys.iter().zip(self.vals.iter())
    }
}

impl Referential for osm::Relation {
    fn indices(&self) -> impl Iterator<Item = ReferenceKey<'_>> {
        self.roles_sid
            .iter()
            .zip(self.memids.iter())
            .zip(self.types.iter())
            .map(|(e, &_v)| Intermediate {
                index: e.1,
                role: e.0,
                #[cfg(debug_assertions)]
                member_type: _v as i32,
            })
    }
}

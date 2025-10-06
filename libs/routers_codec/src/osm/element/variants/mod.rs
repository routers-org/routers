//! Processed element variants

pub mod node;
pub mod relation;
pub mod way;

pub use relation::*;
pub use way::*;

pub mod common {
    #[cfg(debug_assertions)]
    use crate::osm::relation::MemberType;

    use crate::osm::PrimitiveBlock;
    use crate::primitive::Entry;

    use std::str::FromStr;
    use std::{
        collections::HashMap,
        hash::{Hash, Hasher},
        ops::{Add, Deref},
    };

    const OSM_NULL_SENTINEL: i64 = -1i64;

    const VALID_ROADWAYS: [&str; 16] = [
        "motorway",
        "motorway_link",
        "trunk",
        "trunk_link",
        "primary",
        "primary_link",
        "secondary",
        "secondary_link",
        "tertiary",
        "tertiary_link",
        "residential",
        "unclassified",
        // Special Road Types
        "living_street",
        "service",
        "busway",
        "road",
    ];

    #[derive(Clone, Copy, Debug, Eq, PartialOrd, Ord)]
    #[cfg_attr(not(debug_assertions), repr(transparent))]
    pub struct OsmEntryId {
        pub identifier: i64,
        #[cfg(debug_assertions)]
        pub variant: MemberType,
    }

    impl Entry for OsmEntryId {
        #[inline]
        fn identifier(&self) -> i64 {
            self.identifier
        }
    }

    impl Default for OsmEntryId {
        fn default() -> Self {
            OsmEntryId::null()
        }
    }

    impl OsmEntryId {
        pub const fn new(id: i64, #[cfg(debug_assertions)] variant: MemberType) -> OsmEntryId {
            OsmEntryId {
                identifier: id,
                #[cfg(debug_assertions)]
                variant,
            }
        }

        pub const fn null() -> OsmEntryId {
            OsmEntryId {
                identifier: OSM_NULL_SENTINEL,
                #[cfg(debug_assertions)]
                variant: MemberType::Node,
            }
        }

        #[inline]
        pub const fn is_null(&self) -> bool {
            self.identifier == OSM_NULL_SENTINEL
        }

        #[inline]
        pub const fn node(identifier: i64) -> OsmEntryId {
            OsmEntryId {
                identifier,
                #[cfg(debug_assertions)]
                variant: MemberType::Node,
            }
        }

        #[inline]
        pub const fn way(identifier: i64) -> OsmEntryId {
            OsmEntryId {
                identifier,
                #[cfg(debug_assertions)]
                variant: MemberType::Way,
            }
        }
    }

    impl Add<i64> for OsmEntryId {
        type Output = OsmEntryId;

        fn add(self, other: i64) -> Self::Output {
            OsmEntryId {
                identifier: self.identifier + other,
                #[cfg(debug_assertions)]
                variant: self.variant,
            }
        }
    }

    impl From<i64> for OsmEntryId {
        // Defaults to Node variant
        fn from(value: i64) -> Self {
            OsmEntryId::node(value)
        }
    }

    impl PartialEq for OsmEntryId {
        fn eq(&self, other: &Self) -> bool {
            self.identifier == other.identifier
        }
    }

    impl Hash for OsmEntryId {
        #[inline]
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.identifier.hash(state);
        }
    }

    #[derive(Clone, Debug)]
    pub struct Role(pub TagString);

    #[derive(Clone, Debug)]
    pub struct Reference {
        pub id: OsmEntryId,
        pub role: Option<Role>,
    }

    impl Hash for Reference {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.id.hash(state);
        }
    }

    impl PartialEq for Reference {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id
        }
    }

    impl Eq for Reference {}

    impl Reference {
        pub const fn new(id: OsmEntryId, role: Option<Role>) -> Self {
            Reference { id, role }
        }

        #[inline]
        pub const fn without_role(id: OsmEntryId) -> Self {
            Reference { id, role: None }
        }

        #[inline]
        pub const fn with_role(id: OsmEntryId, role: Role) -> Self {
            Reference {
                id,
                role: Some(role),
            }
        }
    }

    #[derive(Clone, Debug)]
    pub struct References(Vec<Reference>);

    /// A reference key is a tuple of the form (Role, MemberID, Type)
    pub type ReferenceKey<'a> = Intermediate<'a>;

    pub struct Intermediate<'a> {
        pub(crate) role: &'a i32,
        pub(crate) index: &'a i64,
        #[cfg(debug_assertions)]
        pub(crate) member_type: &'a i32,
    }

    pub struct IntermediateRole {
        role: Option<Role>,
        index: i64,
        #[cfg(debug_assertions)]
        member_type: MemberType,
    }

    pub trait Referential {
        fn indices(&self) -> impl Iterator<Item = ReferenceKey<'_>>;

        fn references(&self, block: &PrimitiveBlock) -> References {
            self.indices()
                .fold(vec![], |mut prior, intermediate| {
                    #[cfg(debug_assertions)]
                    let Intermediate { member_type, .. } = intermediate;
                    let Intermediate { role, index, .. } = intermediate;

                    let index = index
                        + prior
                            .last()
                            .map_or(&0i64, |IntermediateRole { index, .. }| index);

                    let role = if *role == -1 {
                        None
                    } else {
                        Some(Role(TagString::recover(*role as usize, block)))
                    };

                    #[cfg(debug_assertions)]
                    let member_type =
                        MemberType::try_from(*member_type).unwrap_or(MemberType::Node);

                    prior.push(IntermediateRole {
                        role,
                        index,
                        #[cfg(debug_assertions)]
                        member_type,
                    });

                    prior
                })
                .into_iter()
                // All nodes in a Way are `Node` types, therefore navigable.
                .map(|intermediate| {
                    let entry = OsmEntryId::new(
                        intermediate.index,
                        #[cfg(debug_assertions)]
                        intermediate.member_type,
                    );
                    Reference::new(entry, intermediate.role)
                })
                .collect::<Vec<_>>()
                .into()
        }
    }

    impl Deref for References {
        type Target = Vec<Reference>;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl From<Vec<Reference>> for References {
        fn from(v: Vec<Reference>) -> Self {
            References(v)
        }
    }

    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Hash)]
    pub struct TagString(String);

    impl Deref for TagString {
        type Target = String;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl From<String> for TagString {
        fn from(s: String) -> Self {
            TagString(s)
        }
    }

    impl From<&str> for TagString {
        fn from(s: &str) -> Self {
            TagString(s.to_string())
        }
    }

    impl TagString {
        pub(crate) const HIGHWAY: &'static str = "highway";
        pub(crate) const ONE_WAY: &'static str = "oneway";
        pub(crate) const JUNCTION: &'static str = "junction";
        pub(crate) const LANES: &'static str = "lanes";
        pub(crate) const MAX_SPEED: &'static str = "maxspeed";

        pub fn recover(k: usize, block: &PrimitiveBlock) -> TagString {
            TagString::from(String::from_utf8_lossy(&block.stringtable.s[k]).into_owned())
        }

        pub fn parse<F: FromStr>(&self) -> Option<F> {
            FromStr::from_str(self.as_str()).ok()
        }
    }

    #[derive(Clone, Debug)]
    pub struct Tags(HashMap<TagString, TagString>);

    pub trait Taggable {
        fn indices(&self) -> impl Iterator<Item = (&u32, &u32)>;
        fn tags(&self, block: &PrimitiveBlock) -> Tags {
            Tags::from_block(self.indices(), block)
        }
    }

    impl Tags {
        pub fn new(map: HashMap<TagString, TagString>) -> Self {
            Tags(map)
        }

        /// Takes an iterator of indicies within the string table of the
        /// associated block, and recovers the strings at the specified
        /// indexes, to generate an associative hashmap of the tag keys and values.
        ///
        /// The iterator must yield in the order of (KeyIndex, ValueIndex).
        /// This is most often implemented under the Taggable trait.
        pub fn from_block<'a>(
            iter: impl Iterator<Item = (&'a u32, &'a u32)>,
            block: &PrimitiveBlock,
        ) -> Self {
            Tags(
                iter.map(|(&k, &v)| {
                    (
                        TagString::recover(k as usize, block),
                        TagString::recover(v as usize, block),
                    )
                })
                .collect::<HashMap<TagString, TagString>>(),
            )
        }

        fn r#use(assoc: &str) -> TagString {
            TagString::from(assoc)
        }

        pub(crate) fn get(&self, assoc: &str) -> Option<&TagString> {
            self.0.get(&Tags::r#use(assoc))
        }

        pub(crate) fn r#as<F: FromStr>(&self, assoc: &str) -> Option<F> {
            self.get(assoc).and_then(TagString::parse::<F>)
        }

        #[inline]
        pub fn road_tag(&self) -> Option<&str> {
            self.get(TagString::HIGHWAY)
                .map(|v| v.as_str())
                .filter(|v| VALID_ROADWAYS.contains(v))
        }

        #[inline]
        pub fn one_way(&self) -> bool {
            self.get(TagString::ONE_WAY)
                .is_some_and(|v| v.as_str() == "yes" || v.as_str() == "-1")
        }

        #[inline]
        pub fn roundabout(&self) -> bool {
            self.get(TagString::JUNCTION)
                .is_some_and(|v| v.as_str() == "roundabout" || v.as_str() == "circular")
        }

        // Source: https://wiki.openstreetmap.org/wiki/Default_speed_limits
        // RoadType: oneway
        // TagRules: oneway~yes|-1 or junction~roundabout|circular
        #[inline]
        pub fn unidirectional(&self) -> bool {
            self.one_way() || self.roundabout()
        }
    }

    impl Deref for Tags {
        type Target = HashMap<TagString, TagString>;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
}

pub use common::*;

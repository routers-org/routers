//! Processed element variants

pub mod node;
pub mod relation;
pub mod way;

pub use relation::*;
pub use way::*;

pub mod common {
    use serde::{Deserialize, Serialize};

    use crate::osm::PrimitiveBlock;
    #[cfg(debug_assertions)]
    use crate::osm::relation::MemberType;

    use routers_network::Entry;

    use core::hash::{Hash, Hasher};
    use core::ops::{Add, Deref};
    use core::str::FromStr;
    use std::collections::HashMap;

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

    #[derive(Clone, Copy, Debug, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    #[cfg_attr(not(debug_assertions), repr(transparent))]
    pub struct OsmEntryId {
        pub identifier: i64,
        #[cfg(debug_assertions)]
        variant: i32,
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
                variant: variant as i32,
            }
        }

        pub const fn null() -> OsmEntryId {
            OsmEntryId {
                identifier: OSM_NULL_SENTINEL,
                #[cfg(debug_assertions)]
                variant: MemberType::NODE as i32,
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
                variant: MemberType::NODE as i32,
            }
        }

        #[inline]
        pub const fn way(identifier: i64) -> OsmEntryId {
            OsmEntryId {
                identifier,
                #[cfg(debug_assertions)]
                variant: MemberType::WAY as i32,
            }
        }
    }

    impl Add<i64> for OsmEntryId {
        type Output = OsmEntryId;

        fn add(self, other: i64) -> Self::Output {
            OsmEntryId {
                identifier: self.identifier + other,
                #[cfg(debug_assertions)]
                variant: self.variant as i32,
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

    /// A relation-member role. Owned because the role string is recovered
    /// from the block but `References` flows out of the Way/Relation as an
    /// owned value (Way refs never carry roles — all `role=-1` — so this
    /// allocation only fires for relations, which aren't on the hot path).
    #[derive(Clone, Debug)]
    pub struct Role(pub String);

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
        pub(crate) member_type: i32,
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
                        Some(Role(recover_str(*role as usize, block).to_string()))
                    };

                    #[cfg(debug_assertions)]
                    let member_type = MemberType::from_i32(member_type).unwrap_or(MemberType::NODE);

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

    /// Resolve stringtable index `k` to a `&str` that borrows directly into
    /// the block's bytes. UTF-8 validation runs lazily here — PBF entries
    /// are valid UTF-8 in practice, so this is allocation-free in the
    /// happy path. Out-of-range or invalid UTF-8 falls back to `""`.
    #[inline]
    pub fn recover_str<'b>(k: usize, block: &'b PrimitiveBlock) -> &'b str {
        block
            .stringtable
            .s
            .get(k)
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }

    /// Tag keys and values borrowed from a block's stringtable.
    ///
    /// `Tags<'a>` is a thin wrapper around `HashMap<&'a str, &'a str>` —
    /// no `String` allocations per tag, no `TagString` indirection. Lookups
    /// take `&str` directly via the standard HashMap `Borrow` plumbing.
    #[derive(Clone, Debug)]
    pub struct Tags<'a>(HashMap<&'a str, &'a str>);

    pub trait Taggable {
        fn indices(&self) -> impl Iterator<Item = (&u32, &u32)>;
        fn tags<'b>(&self, block: &'b PrimitiveBlock) -> Tags<'b> {
            Tags::from_block(self.indices(), block)
        }
    }

    impl<'a> Tags<'a> {
        pub(crate) const HIGHWAY: &'static str = "highway";
        pub(crate) const ONE_WAY: &'static str = "oneway";
        pub(crate) const JUNCTION: &'static str = "junction";
        pub(crate) const LANES: &'static str = "lanes";
        pub(crate) const MAX_SPEED: &'static str = "maxspeed";

        pub fn new(map: HashMap<&'a str, &'a str>) -> Self {
            Tags(map)
        }

        /// Takes an iterator of indices into the block's stringtable and
        /// resolves each (key, value) pair as `&str` borrowed straight
        /// from the block's bytes.
        ///
        /// The iterator's items can borrow from any source (typically the
        /// raw `osm::Way`/`osm::Relation` that owns the index arrays) —
        /// the resulting `Tags<'a>` only borrows from the block.
        pub fn from_block<'i, I>(iter: I, block: &'a PrimitiveBlock) -> Self
        where
            I: Iterator<Item = (&'i u32, &'i u32)>,
        {
            Tags(
                iter.map(|(&k, &v)| {
                    (
                        recover_str(k as usize, block),
                        recover_str(v as usize, block),
                    )
                })
                .collect(),
            )
        }

        #[inline]
        pub(crate) fn get(&self, assoc: &str) -> Option<&&'a str> {
            self.0.get(assoc)
        }

        #[inline]
        pub(crate) fn r#as<F: FromStr>(&self, assoc: &str) -> Option<F> {
            self.get(assoc).and_then(|v| F::from_str(v).ok())
        }

        #[inline]
        pub fn road_tag(&self) -> Option<&str> {
            self.get(Tags::HIGHWAY)
                .copied()
                .filter(|v| VALID_ROADWAYS.contains(v))
        }

        #[inline]
        pub fn one_way(&self) -> bool {
            self.get(Tags::ONE_WAY)
                .is_some_and(|&v| v == "yes" || v == "-1")
        }

        #[inline]
        pub fn roundabout(&self) -> bool {
            self.get(Tags::JUNCTION)
                .is_some_and(|&v| v == "roundabout" || v == "circular")
        }

        // Source: https://wiki.openstreetmap.org/wiki/Default_speed_limits
        // RoadType: oneway
        // TagRules: oneway~yes|-1 or junction~roundabout|circular
        #[inline]
        pub fn unidirectional(&self) -> bool {
            self.one_way() || self.roundabout()
        }
    }

    impl<'a> Deref for Tags<'a> {
        type Target = HashMap<&'a str, &'a str>;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
}

pub use common::*;

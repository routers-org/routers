//! The versioned region catalog: the map from geohash cells to the solve
//! regions that own them. (T06)
//!
//! A catalog is a TOML snapshot produced out-of-band by the infrastructure
//! generator and mounted read-only into every orchestrator and matcher. A
//! wrong snapshot would silently misroute traffic — send a vehicle's jobs to a
//! region whose matchers do not hold the right shard — so the whole document
//! is validated the moment it is parsed and never trusted piecemeal
//! afterwards. Lookups are then pure reads against index maps built once at
//! parse time.
//!
//! A **region serves one or more geohash cells** at [`SHARD_PRECISION`], with
//! exactly one shard artifact per covered cell. `coverage` cells are owned and
//! disjoint across the whole catalog; `overlap` cells are certified boundary
//! fallbacks a region can also serve (and may be owned by a different region).
//!
//! # Format
//!
//! ```toml
//! version = 3                  # catalog snapshot version, monotonic
//! routing_version = 3          # bumps whenever point->region mapping changes
//!
//! [[regions]]
//! id = "syd-east"
//! graph = "sydney-2026-09-01"  # GraphVersion; the manifest maps it to files + sha256
//! coverage = ["r3gq", "r3gr"]  # owned cells; disjoint across regions
//! overlap  = ["r3gw"]          # certified fallback cells (may be owned by another region)
//! lanes = 1                    # job lanes 0..lanes
//! resource_class = "cpu-8"
//! replicas = { min = 1, max = 8 }
//! freshness_budget_ms = 30000  # job deadline budget for this region
//! ```
//!
//! Every cell is written as its canonical base-32 string. That differs from
//! [`Geohash`]'s derived serde form (`{ data, precision }`), which is opaque
//! in a hand-edited file, so cells go through [`Display`](core::fmt::Display)/[`FromStr`](core::str::FromStr) instead
//! (see the private `cells` module).

use core::time::Duration;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use routers_shard::Geohash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::SHARD_PRECISION;
use crate::protocol::ids::{GraphVersion, RegionId, token_safe};

/// The horizontal-scale bounds the scheduler is allowed to run a region
/// between. `min` guarantees a floor of warm matchers; `max` caps the burst.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replicas {
    /// Fewest matcher replicas to keep serving the region (at least one).
    pub min: u32,
    /// Most matcher replicas the region may scale out to.
    pub max: u32,
}

/// One solve region: a graph snapshot, the cells it owns, and the operational
/// envelope the orchestrator and autoscaler need to run it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    /// The region's identity; also a NATS subject and stream token, so it must
    /// be [`token_safe`].
    pub id: RegionId,
    /// The road-network snapshot this region's matchers load and solve against.
    pub graph: GraphVersion,
    /// The owned cells. Disjoint across the catalog; one shard artifact each.
    #[serde(with = "cells")]
    pub coverage: Vec<Geohash>,
    /// Certified fallback cells this region can also serve at a boundary. May
    /// be owned (in `coverage`) by another region; never by this one.
    #[serde(default, with = "cells")]
    pub overlap: Vec<Geohash>,
    /// How many priority lanes the region's job plane exposes: lanes `0..lanes`
    /// are valid, so this is at least one.
    pub lanes: u8,
    /// The compute profile the scheduler places matchers on (an opaque label
    /// resolved by the infrastructure layer, e.g. `"cpu-8"`).
    pub resource_class: String,
    /// The replica scaling envelope.
    pub replicas: Replicas,
    /// The per-job deadline budget for this region. Serialised as whole
    /// milliseconds (`freshness_budget_ms`) because that is what a job's
    /// `deadline_us` is derived from.
    #[serde(rename = "freshness_budget_ms", with = "duration_ms")]
    pub freshness_budget: Duration,
}

/// A versioned snapshot of the whole region topology.
///
/// Construct one with [`Catalog::parse`] or [`Catalog::load`]; both validate
/// the document and build the private lookup indices. The data fields are
/// public for inspection but the type cannot be assembled field-by-field, so a
/// `Catalog` in hand is always a validated, indexed one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// The snapshot's monotonic version. Lets a consumer detect a newer mount.
    pub version: u64,
    /// Bumps whenever the point-to-region mapping changes, so an orchestrator
    /// can refuse to mix results computed against different routings.
    pub routing_version: u64,
    /// The regions, in the order the source document listed them.
    #[serde(default)]
    pub regions: Vec<Region>,

    // Lookup indices, rebuilt from `regions` at parse time and never
    // serialised. Kept private so they cannot drift from `regions`.
    /// Region id -> index into `regions`.
    #[serde(skip)]
    region_index: HashMap<RegionId, usize>,
    /// Owned cell -> index of its owning region.
    #[serde(skip)]
    owner_index: HashMap<Geohash, usize>,
    /// Overlap cell -> indices of the regions offering it as a fallback.
    #[serde(skip)]
    fallback_index: HashMap<Geohash, Vec<usize>>,
}

/// Everything a catalog can be rejected for. Each variant names the offending
/// region (and cell, where relevant) so an operator can find it in the source.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// The catalog file could not be read.
    #[error("failed to read catalog from {path:?}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// The bytes were not valid catalog TOML (syntax, a missing required key,
    /// or a malformed geohash string).
    #[error("failed to parse catalog TOML: {0}")]
    Toml(#[from] toml::de::Error),
    /// The catalog defined no regions; nothing could be served.
    #[error("catalog has no regions")]
    EmptyRegions,
    /// Two regions shared an id, so it no longer identifies one region.
    #[error("region {0:?} is defined more than once")]
    DuplicateRegion(String),
    /// An id or graph value held a character that is unsafe in a NATS subject.
    #[error("region {region:?} has an unsafe {field} token {value:?} (needs [A-Za-z0-9_-]+)")]
    UnsafeToken {
        /// The region the offending token belongs to.
        region: String,
        /// Which field held it: `"id"` or `"graph"`.
        field: &'static str,
        /// The rejected value.
        value: String,
    },
    /// A cell was not at [`SHARD_PRECISION`], so it names no shard artifact.
    #[error("region {region:?} cell {cell:?} has precision {precision}, expected {expected}")]
    CellPrecision {
        /// The region the cell was listed under.
        region: String,
        /// The offending cell, as its base-32 string.
        cell: String,
        /// The precision it was written at.
        precision: u8,
        /// The precision every cell must use.
        expected: u8,
    },
    /// One cell appeared in the `coverage` of two regions; ownership must be
    /// unambiguous.
    #[error("cell {cell:?} is owned by both region {first:?} and region {second:?}")]
    CellConflict {
        /// The doubly-owned cell.
        cell: String,
        /// The region that claimed it first.
        first: String,
        /// The region that claimed it again.
        second: String,
    },
    /// A region exposed no lanes, so no job could ever be addressed to it.
    #[error("region {0:?} sets lanes = 0 (needs at least one lane)")]
    ZeroLanes(String),
    /// A region's replica envelope was empty or inverted.
    #[error("region {region:?} has invalid replicas (min={min}, max={max}); need 1 <= min <= max")]
    BadReplicas {
        /// The offending region.
        region: String,
        /// The configured minimum.
        min: u32,
        /// The configured maximum.
        max: u32,
    },
    /// A region owned no cells, so it would serve nothing.
    #[error("region {0:?} has empty coverage")]
    EmptyCoverage(String),
    /// A region listed one of its own owned cells as an overlap fallback, which
    /// is contradictory.
    #[error("region {region:?} lists cell {cell:?} as both coverage and overlap")]
    OverlapOwnCell {
        /// The offending region.
        region: String,
        /// The cell listed twice.
        cell: String,
    },
    /// A region had a zero freshness budget, which cannot yield a job deadline.
    #[error("region {0:?} has a zero freshness budget")]
    ZeroFreshnessBudget(String),
}

impl Catalog {
    /// Parse and fully validate a catalog from a TOML string, building the
    /// lookup indices. Every failure the document can carry is reported as a
    /// [`CatalogError`]; the first one found is returned.
    pub fn parse(toml: &str) -> Result<Self, CatalogError> {
        let mut catalog: Catalog = toml::from_str(toml)?;
        catalog.validate_and_index()?;
        Ok(catalog)
    }

    /// Read a catalog from `path` and [`parse`](Self::parse) it. An I/O failure
    /// is distinguished from a content failure by the error variant.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CatalogError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| CatalogError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text)
    }

    /// The region with this id, if any. O(1).
    pub fn region(&self, id: &RegionId) -> Option<&Region> {
        self.region_index.get(id).map(|&idx| &self.regions[idx])
    }

    /// The region that owns `cell` in its `coverage`, if any. O(1).
    pub fn owner_of(&self, cell: &Geohash) -> Option<&Region> {
        self.owner_index.get(cell).map(|&idx| &self.regions[idx])
    }

    /// The regions offering `cell` as a certified `overlap` fallback, in
    /// catalog order. Empty if none list it. O(1) to locate, then yields each
    /// candidate.
    pub fn fallbacks_for(&self, cell: &Geohash) -> impl Iterator<Item = &Region> {
        self.fallback_index
            .get(cell)
            .into_iter()
            .flatten()
            .map(move |&idx| &self.regions[idx])
    }

    /// Every owned cell paired with its owning region, in catalog order.
    pub fn cells(&self) -> impl Iterator<Item = (&Geohash, &Region)> {
        self.regions
            .iter()
            .flat_map(|region| region.coverage.iter().map(move |cell| (cell, region)))
    }

    /// Run the full validation pass and populate the private indices. Ordered
    /// so a report points at one clear cause: per-region shape first (in
    /// document order), then the cross-region invariants (unique ids, disjoint
    /// ownership) as the indices are assembled.
    fn validate_and_index(&mut self) -> Result<(), CatalogError> {
        if self.regions.is_empty() {
            return Err(CatalogError::EmptyRegions);
        }

        for region in &self.regions {
            region.validate_shape()?;
        }

        let mut region_index = HashMap::with_capacity(self.regions.len());
        let mut owner_index: HashMap<Geohash, usize> = HashMap::new();
        let mut fallback_index: HashMap<Geohash, Vec<usize>> = HashMap::new();

        for (idx, region) in self.regions.iter().enumerate() {
            if region_index.insert(region.id.clone(), idx).is_some() {
                return Err(CatalogError::DuplicateRegion(region.id.as_str().to_owned()));
            }
            for cell in &region.coverage {
                if let Some(&first) = owner_index.get(cell) {
                    return Err(CatalogError::CellConflict {
                        cell: cell.to_string(),
                        first: self.regions[first].id.as_str().to_owned(),
                        second: region.id.as_str().to_owned(),
                    });
                }
                owner_index.insert(*cell, idx);
            }
            for cell in &region.overlap {
                fallback_index.entry(*cell).or_default().push(idx);
            }
        }

        self.region_index = region_index;
        self.owner_index = owner_index;
        self.fallback_index = fallback_index;
        Ok(())
    }
}

impl Region {
    /// Validate everything that depends only on this region in isolation. The
    /// cross-region invariants (unique id, disjoint coverage) are checked by
    /// [`Catalog::validate_and_index`] while it builds the indices.
    fn validate_shape(&self) -> Result<(), CatalogError> {
        let id = self.id.as_str();
        if !token_safe(id) {
            return Err(CatalogError::UnsafeToken {
                region: id.to_owned(),
                field: "id",
                value: id.to_owned(),
            });
        }
        if !token_safe(self.graph.as_str()) {
            return Err(CatalogError::UnsafeToken {
                region: id.to_owned(),
                field: "graph",
                value: self.graph.as_str().to_owned(),
            });
        }
        if self.coverage.is_empty() {
            return Err(CatalogError::EmptyCoverage(id.to_owned()));
        }
        if self.lanes == 0 {
            return Err(CatalogError::ZeroLanes(id.to_owned()));
        }
        if self.replicas.min == 0 || self.replicas.min > self.replicas.max {
            return Err(CatalogError::BadReplicas {
                region: id.to_owned(),
                min: self.replicas.min,
                max: self.replicas.max,
            });
        }
        if self.freshness_budget.is_zero() {
            return Err(CatalogError::ZeroFreshnessBudget(id.to_owned()));
        }
        for cell in self.coverage.iter().chain(self.overlap.iter()) {
            if cell.precision != SHARD_PRECISION {
                return Err(CatalogError::CellPrecision {
                    region: id.to_owned(),
                    cell: cell.to_string(),
                    precision: cell.precision,
                    expected: SHARD_PRECISION,
                });
            }
        }
        for cell in &self.overlap {
            if self.coverage.contains(cell) {
                return Err(CatalogError::OverlapOwnCell {
                    region: id.to_owned(),
                    cell: cell.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// (De)serialise a `Vec<Geohash>` as a list of canonical base-32 strings, so a
/// catalog file reads `coverage = ["r3gq"]` rather than exposing the geohash's
/// internal `{ data, precision }` representation. Precision is checked later,
/// with the region's id to hand, by [`Region::validate_shape`].
mod cells {
    use core::str::FromStr;

    use routers_shard::Geohash;
    use serde::{Deserialize, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(
        cells: &[Geohash],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        cells
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Geohash>, D::Error> {
        let raw = Vec::<String>::deserialize(deserializer)?;
        raw.into_iter()
            .map(|cell| Geohash::from_str(&cell).map_err(serde::de::Error::custom))
            .collect()
    }
}

/// (De)serialise a [`Duration`] as whole milliseconds, matching the
/// `freshness_budget_ms` key an operator edits by hand.
mod duration_ms {
    use core::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        // Budgets are seconds-scale, so this saturation is unreachable; it only
        // keeps a pathological value from panicking on the cast.
        let millis = u64::try_from(value.as_millis()).unwrap_or(u64::MAX);
        serializer.serialize_u64(millis)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use super::*;

    /// The example from the module docs, kept in step with them.
    const EXAMPLE: &str = r#"
version = 3
routing_version = 3

[[regions]]
id = "syd-east"
graph = "sydney-2026-09-01"
coverage = ["r3gq", "r3gr"]
overlap  = ["r3gw"]
lanes = 1
resource_class = "cpu-8"
replicas = { min = 1, max = 8 }
freshness_budget_ms = 30000
"#;

    fn cell(s: &str) -> Geohash {
        Geohash::from_str(s).unwrap()
    }

    fn region_id(s: &str) -> RegionId {
        RegionId::new(s).unwrap()
    }

    #[test]
    fn documented_example_parses() {
        let catalog = Catalog::parse(EXAMPLE).expect("documented example should parse");
        assert_eq!(catalog.version, 3);
        assert_eq!(catalog.routing_version, 3);
        assert_eq!(catalog.regions.len(), 1);

        let region = &catalog.regions[0];
        assert_eq!(region.id.as_str(), "syd-east");
        assert_eq!(region.graph.as_str(), "sydney-2026-09-01");
        assert_eq!(region.coverage, vec![cell("r3gq"), cell("r3gr")]);
        assert_eq!(region.overlap, vec![cell("r3gw")]);
        assert_eq!(region.lanes, 1);
        assert_eq!(region.resource_class, "cpu-8");
        assert_eq!(region.replicas, Replicas { min: 1, max: 8 });
        assert_eq!(region.freshness_budget, Duration::from_millis(30_000));
    }

    #[test]
    fn overlap_defaults_to_empty_when_omitted() {
        let toml = r#"
version = 1
routing_version = 1

[[regions]]
id = "r"
graph = "g"
coverage = ["r3gq"]
lanes = 1
resource_class = "cpu-1"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#;
        let catalog = Catalog::parse(toml).unwrap();
        assert!(catalog.regions[0].overlap.is_empty());
    }

    #[test]
    fn validation_errors_are_specific() {
        // Each case is the smallest edit to a valid catalog that trips exactly
        // one rule. The predicate pins the variant; several also assert the
        // offending id/cell reaches the message.
        struct Case {
            label: &'static str,
            toml: &'static str,
            want: fn(&CatalogError) -> bool,
        }

        let cases = [
            Case {
                label: "empty regions",
                toml: "version = 1\nrouting_version = 1\n",
                want: |e| matches!(e, CatalogError::EmptyRegions),
            },
            Case {
                label: "duplicate region id",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "dup"
graph = "g"
coverage = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
[[regions]]
id = "dup"
graph = "g"
coverage = ["r3gr"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#,
                want: |e| matches!(e, CatalogError::DuplicateRegion(id) if id == "dup"),
            },
            Case {
                label: "unsafe id token",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "bad.id"
graph = "g"
coverage = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#,
                want: |e| {
                    matches!(
                        e,
                        CatalogError::UnsafeToken { field: "id", value, .. } if value == "bad.id"
                    )
                },
            },
            Case {
                label: "unsafe graph token",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "bad graph"
coverage = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#,
                want: |e| matches!(e, CatalogError::UnsafeToken { field: "graph", .. }),
            },
            Case {
                label: "cell precision mismatch",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = ["r3g"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#,
                want: |e| {
                    matches!(
                        e,
                        CatalogError::CellPrecision { cell, precision: 3, expected: 4, .. }
                            if cell == "r3g"
                    )
                },
            },
            Case {
                label: "cell owned by two regions",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "a"
graph = "g"
coverage = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
[[regions]]
id = "b"
graph = "g"
coverage = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#,
                want: |e| {
                    matches!(
                        e,
                        CatalogError::CellConflict { cell, first, second }
                            if cell == "r3gq" && first == "a" && second == "b"
                    )
                },
            },
            Case {
                label: "zero lanes",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = ["r3gq"]
lanes = 0
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#,
                want: |e| matches!(e, CatalogError::ZeroLanes(id) if id == "r"),
            },
            Case {
                label: "replicas min > max",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 5, max = 2 }
freshness_budget_ms = 1000
"#,
                want: |e| matches!(e, CatalogError::BadReplicas { min: 5, max: 2, .. }),
            },
            Case {
                label: "replicas min == 0",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 0, max = 2 }
freshness_budget_ms = 1000
"#,
                want: |e| matches!(e, CatalogError::BadReplicas { min: 0, .. }),
            },
            Case {
                label: "empty coverage",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = []
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#,
                want: |e| matches!(e, CatalogError::EmptyCoverage(id) if id == "r"),
            },
            Case {
                label: "overlap equals own coverage cell",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = ["r3gq"]
overlap = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#,
                want: |e| {
                    matches!(
                        e,
                        CatalogError::OverlapOwnCell { region, cell }
                            if region == "r" && cell == "r3gq"
                    )
                },
            },
            Case {
                label: "zero freshness budget",
                toml: r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = ["r3gq"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 0
"#,
                want: |e| matches!(e, CatalogError::ZeroFreshnessBudget(id) if id == "r"),
            },
        ];

        for case in cases {
            match Catalog::parse(case.toml) {
                Err(err) if (case.want)(&err) => {}
                other => panic!("{}: unexpected result {other:?}", case.label),
            }
        }
    }

    #[test]
    fn overlap_precision_is_validated_too() {
        // Coverage is well-formed; the mismatch is in overlap, proving the
        // precision check spans both lists.
        let toml = r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = ["r3gq"]
overlap = ["r3g"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#;
        assert!(matches!(
            Catalog::parse(toml),
            Err(CatalogError::CellPrecision { cell, .. }) if cell == "r3g"
        ));
    }

    #[test]
    fn malformed_geohash_is_a_toml_error() {
        // 'a' and 'i' are not in the geohash base-32 alphabet, so this fails in
        // deserialisation rather than the semantic pass.
        let toml = r#"
version = 1
routing_version = 1
[[regions]]
id = "r"
graph = "g"
coverage = ["ailq"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#;
        assert!(matches!(Catalog::parse(toml), Err(CatalogError::Toml(_))));
    }

    #[test]
    fn lookups_resolve_by_id_cell_and_fallback() {
        let toml = r#"
version = 2
routing_version = 2
[[regions]]
id = "west"
graph = "g"
coverage = ["r3gq", "r3gr"]
overlap = ["r3gw"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
[[regions]]
id = "east"
graph = "g"
coverage = ["r3gw"]
overlap = ["r3gr"]
lanes = 1
resource_class = "c"
replicas = { min = 1, max = 1 }
freshness_budget_ms = 1000
"#;
        let catalog = Catalog::parse(toml).unwrap();

        // by id
        assert_eq!(
            catalog.region(&region_id("west")).map(|r| r.id.as_str()),
            Some("west")
        );
        assert!(catalog.region(&region_id("nowhere")).is_none());

        // by owned cell
        assert_eq!(
            catalog.owner_of(&cell("r3gq")).map(|r| r.id.as_str()),
            Some("west")
        );
        assert_eq!(
            catalog.owner_of(&cell("r3gw")).map(|r| r.id.as_str()),
            Some("east")
        );
        assert!(catalog.owner_of(&cell("r3gz")).is_none());

        // fallbacks: r3gw is owned by east but offered as a fallback by west
        let fallbacks: Vec<&str> = catalog
            .fallbacks_for(&cell("r3gw"))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(fallbacks, vec!["west"]);
        // r3gr is owned by west and offered as a fallback by east
        let fallbacks: Vec<&str> = catalog
            .fallbacks_for(&cell("r3gr"))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(fallbacks, vec!["east"]);
        assert_eq!(catalog.fallbacks_for(&cell("r3gz")).count(), 0);

        // every owned cell, in catalog order
        let owned: Vec<(String, &str)> = catalog
            .cells()
            .map(|(c, r)| (c.to_string(), r.id.as_str()))
            .collect();
        assert_eq!(
            owned,
            vec![
                ("r3gq".to_owned(), "west"),
                ("r3gr".to_owned(), "west"),
                ("r3gw".to_owned(), "east"),
            ]
        );
    }

    #[test]
    fn serialise_round_trips_through_parse() {
        let catalog = Catalog::parse(EXAMPLE).unwrap();
        let rendered = toml::to_string(&catalog).expect("catalog should serialise");
        let reparsed = Catalog::parse(&rendered).expect("re-rendered catalog should parse");
        assert_eq!(catalog, reparsed);
    }

    /// A unique path under the temp dir that does not yet exist. Namespaced by
    /// process id and a per-call counter so parallel tests never collide,
    /// without reaching for a wall clock.
    fn scratch_path(tag: &str) -> PathBuf {
        use core::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "routers-catalog-{tag}-{}-{seq}.toml",
            std::process::id()
        ));
        path
    }

    #[test]
    fn load_reads_and_parses_a_file() {
        // `load` is just `read_to_string` + `parse`, so a file written from the
        // documented example must load to the same catalog `parse` produces.
        let path = scratch_path("load");
        std::fs::write(&path, EXAMPLE).expect("write scratch catalog");
        let loaded = Catalog::load(&path).expect("written catalog should load");
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, Catalog::parse(EXAMPLE).unwrap());
    }

    #[test]
    fn load_missing_path_is_an_io_error() {
        // A read failure is surfaced as `Io` (carrying the path), distinct from
        // a content failure, so an operator can tell a missing mount from a bad
        // one.
        let path = scratch_path("missing");
        let _ = std::fs::remove_file(&path);
        match Catalog::load(&path) {
            Err(CatalogError::Io { path: reported, .. }) => assert_eq!(reported, path),
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}

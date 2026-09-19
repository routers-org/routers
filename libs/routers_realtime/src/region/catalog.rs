//! The versioned region catalog: the map from geohash cells to the solve
//! regions that own them.
//!
//! A [`Catalog`] is a TOML snapshot mounted read-only into every orchestrator
//! and matcher. It is fully validated at parse time and never trusted
//! piecemeal; lookups are then pure O(1) reads against indices built once.
//! `coverage` cells are owned and disjoint across the catalog; `overlap` cells
//! are certified boundary fallbacks a region can also serve.

use core::time::Duration;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use routers_shard::Geohash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::SHARD_PRECISION;
use crate::protocol::ids::{GraphVersion, RegionId};

/// The horizontal-scale bounds the scheduler is allowed to run a region
/// between. `min` guarantees a floor of warm matchers; `max` caps the burst.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replicas {
    /// Fewest matcher replicas to keep serving the region (at least one).
    pub min: u32,
    /// Most matcher replicas the region may scale out to.
    pub max: u32,
}

/// One solve region: a graph snapshot, the cells it owns, and the operational
/// envelope the orchestrator and autoscaler need to run it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    /// The region's identity; also a NATS subject/stream token.
    pub id: RegionId,
    /// The road-network snapshot this region's matchers load and solve against.
    pub graph: GraphVersion,
    /// The owned cells. Disjoint across the catalog; one shard artifact each.
    #[serde(with = "cells")]
    pub coverage: Vec<Geohash>,
    /// Certified fallback cells this region can also serve; may be owned by another region, never this one.
    #[serde(default, with = "cells")]
    pub overlap: Vec<Geohash>,
    /// Priority lanes the job plane exposes; lanes `0..lanes` are valid, so at least one.
    pub lanes: u8,
    /// The compute profile the scheduler places matchers on (opaque label, e.g. `"cpu-8"`).
    pub resource_class: String,
    /// The replica scaling envelope.
    pub replicas: Replicas,
    /// The per-job deadline budget, serialised as whole milliseconds (`freshness_budget_ms`).
    #[serde(rename = "freshness_budget_ms", with = "duration_ms")]
    pub freshness_budget: Duration,
}

/// A versioned snapshot of the whole region topology.
///
/// Every deserialization route, including direct `toml` or `serde` use,
/// validates the document and builds its indices before yielding a value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawCatalog")]
pub struct Catalog {
    /// The snapshot's monotonic version. Lets a consumer detect a newer mount.
    pub version: u64,
    /// Bumps whenever point-to-region mapping changes, so consumers can refuse to mix routings.
    pub routing_version: u64,
    /// The regions, in the order the source document listed them.
    #[serde(default)]
    pub regions: Vec<Region>,

    // Lookup indices rebuilt from `regions` at parse time; never serialised.
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

/// The serialised catalog shape before cross-region validation and indexing.
/// Keeping it private ensures only [`Catalog`] crosses the public boundary.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    version: u64,
    routing_version: u64,
    #[serde(default)]
    regions: Vec<Region>,
}

/// Everything a catalog can be rejected for. Each variant names the offending
/// region (and cell, where relevant) so an operator can find it in the source.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// The catalog file could not be read.
    #[error("failed to read catalog from {path:?}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The bytes were not valid catalog TOML.
    #[error("failed to parse catalog TOML: {0}")]
    Toml(#[from] toml::de::Error),
    /// The catalog defined no regions; nothing could be served.
    #[error("catalog has no regions")]
    EmptyRegions,
    /// Two regions shared an id, so it no longer identifies one region.
    #[error("region {0:?} is defined more than once")]
    DuplicateRegion(String),
    /// A cell was not at [`SHARD_PRECISION`], so it names no shard artifact.
    #[error("region {region:?} cell {cell:?} has precision {precision}, expected {expected}")]
    CellPrecision {
        region: String,
        cell: String,
        precision: u8,
        expected: u8,
    },
    /// One cell appeared in the `coverage` of two regions; ownership must be unambiguous.
    #[error("cell {cell:?} is owned by both region {first:?} and region {second:?}")]
    CellConflict {
        cell: String,
        first: String,
        second: String,
    },
    /// A region exposed no lanes, so no job could ever be addressed to it.
    #[error("region {0:?} sets lanes = 0 (needs at least one lane)")]
    ZeroLanes(String),
    /// A region's replica envelope was empty or inverted.
    #[error("region {region:?} has invalid replicas (min={min}, max={max}); need 1 <= min <= max")]
    BadReplicas { region: String, min: u32, max: u32 },
    /// A region owned no cells, so it would serve nothing.
    #[error("region {0:?} has empty coverage")]
    EmptyCoverage(String),
    /// A region listed one of its own owned cells as an overlap fallback.
    #[error("region {region:?} lists cell {cell:?} as both coverage and overlap")]
    OverlapOwnCell { region: String, cell: String },
    /// A region had a zero freshness budget, which cannot yield a job deadline.
    #[error("region {0:?} has a zero freshness budget")]
    ZeroFreshnessBudget(String),
}

impl Catalog {
    /// Parse and fully validate a catalog from TOML, building the lookup
    /// indices. The first [`CatalogError`] found is returned.
    pub fn parse(toml: &str) -> Result<Self, CatalogError> {
        let raw: RawCatalog = toml::from_str(toml)?;
        raw.try_into()
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

    /// The regions offering `cell` as an `overlap` fallback, in catalog order. Empty if none.
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

    /// Run the full validation pass and populate the private indices.
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

impl TryFrom<RawCatalog> for Catalog {
    type Error = CatalogError;

    fn try_from(raw: RawCatalog) -> Result<Self, Self::Error> {
        let mut catalog = Self {
            version: raw.version,
            routing_version: raw.routing_version,
            regions: raw.regions,
            region_index: HashMap::new(),
            owner_index: HashMap::new(),
            fallback_index: HashMap::new(),
        };
        catalog.validate_and_index()?;
        Ok(catalog)
    }
}

impl Region {
    /// Validate everything that depends only on this region in isolation.
    fn validate_shape(&self) -> Result<(), CatalogError> {
        let id = self.id.as_str();
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

/// (De)serialise a `Vec<Geohash>` as canonical base-32 strings for a hand-editable file.
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

/// (De)serialise a [`Duration`] as whole milliseconds (`freshness_budget_ms`).
mod duration_ms {
    use core::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        // Budgets are seconds-scale, so this saturation is unreachable.
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

        assert_eq!(
            catalog.region(&region_id("west")).map(|r| r.id.as_str()),
            Some("west")
        );
        assert!(catalog.region(&region_id("nowhere")).is_none());

        assert_eq!(
            catalog.owner_of(&cell("r3gq")).map(|r| r.id.as_str()),
            Some("west")
        );
        assert_eq!(
            catalog.owner_of(&cell("r3gw")).map(|r| r.id.as_str()),
            Some("east")
        );
        assert!(catalog.owner_of(&cell("r3gz")).is_none());

        let fallbacks: Vec<&str> = catalog
            .fallbacks_for(&cell("r3gw"))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(fallbacks, vec!["west"]);
        let fallbacks: Vec<&str> = catalog
            .fallbacks_for(&cell("r3gr"))
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(fallbacks, vec!["east"]);
        assert_eq!(catalog.fallbacks_for(&cell("r3gz")).count(), 0);

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

    #[test]
    fn direct_deserialization_validates_indexes_and_round_trips() {
        let catalog: Catalog = toml::from_str(EXAMPLE).expect("direct TOML deserialization");
        assert_eq!(
            catalog
                .region(&region_id("syd-east"))
                .map(|region| region.graph.as_str()),
            Some("sydney-2026-09-01")
        );
        assert_eq!(
            catalog
                .owner_of(&cell("r3gq"))
                .map(|region| region.id.as_str()),
            Some("syd-east")
        );
        assert_eq!(
            catalog
                .fallbacks_for(&cell("r3gw"))
                .map(|region| region.id.as_str())
                .collect::<Vec<_>>(),
            vec!["syd-east"]
        );

        let bytes = postcard::to_allocvec(&catalog).expect("catalog serializes");
        let decoded: Catalog = postcard::from_bytes(&bytes).expect("catalog deserializes");
        assert_eq!(decoded, catalog);
        assert_eq!(
            decoded
                .owner_of(&cell("r3gr"))
                .map(|region| region.id.as_str()),
            Some("syd-east")
        );
    }

    #[test]
    fn direct_deserialization_rejects_invalid_catalogs() {
        let invalid_token = EXAMPLE.replace("id = \"syd-east\"", "id = \"bad.id\"");
        assert!(toml::from_str::<Catalog>(&invalid_token).is_err());

        let unknown_top_level = format!("{EXAMPLE}\nunexpected = true\n");
        assert!(toml::from_str::<Catalog>(&unknown_top_level).is_err());

        let unknown_region_field = EXAMPLE.replace("lanes = 1", "lanes = 1\nunexpected = true");
        assert!(toml::from_str::<Catalog>(&unknown_region_field).is_err());

        let mut raw: RawCatalog = toml::from_str(EXAMPLE).expect("raw catalog parses for attack");
        raw.regions[0].lanes = 0;
        let bytes = postcard::to_allocvec(&raw).expect("raw catalog serializes");
        assert!(postcard::from_bytes::<Catalog>(&bytes).is_err());
    }

    /// A unique scratch path, namespaced by pid and a counter so parallel tests never collide.
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
        let path = scratch_path("load");
        std::fs::write(&path, EXAMPLE).expect("write scratch catalog");
        let loaded = Catalog::load(&path).expect("written catalog should load");
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, Catalog::parse(EXAMPLE).unwrap());
    }

    #[test]
    fn load_missing_path_is_an_io_error() {
        let path = scratch_path("missing");
        let _ = std::fs::remove_file(&path);
        match Catalog::load(&path) {
            Err(CatalogError::Io { path: reported, .. }) => assert_eq!(reported, path),
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}

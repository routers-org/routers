// `core::io::ErrorKind` is unstable, so `std::io::ErrorKind` is the only stable
// path and `clippy::std_instead_of_core` cannot be followed here.
#![allow(clippy::std_instead_of_core)]

use clap::{Args as ClapArgs, Parser};
use geo::Point;
use log::{debug, error, info};
use std::path::{Path, PathBuf};
use web_time::{SystemTime, UNIX_EPOCH};

extern crate alloc;
use alloc::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_network::edge::Weight;
use routers_shard::{GeohashStrategy, ShardSource, ShardedNetwork};

/// The JSON artifact manifest's file name, written beside the `.shard.rt` bundles.
const JSON_MANIFEST_FILENAME: &str = "manifest.json";

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The path to the PBF/RT file to load.
    #[command(flatten)]
    file: FileInput,

    /// The precision of the geohash strategy to use.
    #[arg(short, long, env, default_value = "4")]
    precision: u8,

    /// The graph snapshot version these shards are built for (a NATS-safe token,
    /// e.g. `sydney-2026-09-01`). Recorded in `manifest.json` so a matcher rejects
    /// a bundle built for a different graph.
    #[arg(long, env = "GRAPH_VERSION")]
    graph_version: String,

    /// Metres of cross-boundary buffer admitted around each shard.
    #[arg(long, env = "PADDING_DISTANCE", default_value = "1000.0")]
    padding: f64,

    /// Only write shards whose id starts with this prefix. For chunked
    /// extracts: a chunk cut with a buffer around a geohash cell yields
    /// shards outside that cell too, which belong to the neighbouring chunk.
    #[arg(long, env = "ONLY_PREFIX")]
    only_prefix: Option<String>,

    /// The output directory to write shard files to. Defaults to the
    /// workspace's `target/shard_cache` (what the chart mounts).
    #[arg(short, long, env = "SHARD_OUTPUT_DIR")]
    output: Option<PathBuf>,

    /// The name of the manifest file to write. An existing manifest is
    /// merged into, not replaced, so several regions can share one output
    /// directory.
    #[arg(short, long, env = "MANIFEST_FILENAME", default_value = "manifest.txt")]
    manifest_filename: String,
}

#[derive(ClapArgs, Debug)]
#[group(required = true, multiple = false)]
struct FileInput {
    /// The path to the PBF file to load.
    #[arg(long)]
    pbf: Option<PathBuf>,

    /// The path to the RT file to load.
    #[arg(long)]
    rt: Option<PathBuf>,
}

fn main() {
    env_logger::init();

    let args = Args::parse();
    info!("generate-shards starting: {:?}", args);

    if !token_safe(&args.graph_version) {
        error!(
            "graph version {:?} is not a NATS-safe token (needs [A-Za-z0-9_-]+)",
            args.graph_version
        );
        std::process::exit(1);
    }

    let out_dir = args.output.unwrap_or_else(|| {
        // `cargo run` sets CARGO_MANIFEST_DIR to libs/routers_shard, so the
        // fallback lands in the workspace's target/shard_cache.
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default())
            .join("../../target/shard_cache")
    });
    std::fs::create_dir_all(&out_dir).expect("create output dir");

    let network = match (args.file.pbf, args.file.rt) {
        (Some(pbf), None) => {
            info!("loading OsmNetwork from protobuf file...");
            OsmNetwork::from_pbf(&pbf).map_err(|v| v.to_string())
        }
        (None, Some(rt)) => {
            info!("loading OsmNetwork from cached (.rt) file...");
            OsmNetwork::from_saved(&rt)
        }
        _ => unreachable!(),
    }
    .expect("must be able to parse the provided file");

    debug!(
        "file contained {} nodes, {} edges",
        network.hash.len(),
        network.graph.edge_count()
    );

    let strategy = GeohashStrategy::with_precision(args.precision);
    let source = OsmSource(&network);

    // Indices are not serialised, so building them here would be wasted.
    let mut partition = ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, _>::partition(
        &source,
        &strategy,
        args.padding,
    )
    .without_indices();
    let dealt = partition.len();
    if let Some(prefix) = &args.only_prefix {
        partition.retain(|id| id.to_string().starts_with(prefix));
        info!(
            "dealt into {dealt} shards, keeping the {} under prefix {prefix:?}",
            partition.len()
        );
    }
    let total = partition.len();
    info!("writing {total} shards to {out_dir:?}");

    let built_at = rfc3339_utc(now());
    let mut built = Vec::with_capacity(total);
    let mut artifacts: BTreeMap<String, ArtifactDoc> = BTreeMap::new();
    let mut failed = Vec::new();
    for (i, net) in partition.enumerate() {
        let name = format!("{}.shard.rt", net.owned);
        let cell = net.owned.to_string();
        debug!("[{} / {total}] {net:?}", i + 1);
        let path = out_dir.join(&name);
        if let Err(e) = net.save_to_file(&path) {
            error!("[{} / {total}] failed to save {name}: {e}", i + 1);
            failed.push((name, e));
            continue;
        }
        // A checksum failure is surfaced but still keeps the shard in the `.txt` list.
        built.push(name.clone());
        match artifact_metadata(&path) {
            Ok((sha256, bytes)) => {
                artifacts.insert(
                    cell,
                    ArtifactDoc {
                        file: name,
                        sha256,
                        bytes,
                        graph: args.graph_version.clone(),
                        precision: args.precision,
                        nodes: net.num_nodes() as u64,
                        edges: net.num_edges() as u64,
                        built_at: built_at.clone(),
                    },
                );
            }
            Err(e) => error!("[{} / {total}] failed to checksum {name}: {e}", i + 1),
        }
    }

    // Merge into any existing manifest: the output directory is shared by
    // every region generated into it, and a rerun must not duplicate lines.
    let manifest = out_dir.join(args.manifest_filename);
    let mut names: BTreeSet<String> = match std::fs::read_to_string(&manifest) {
        Ok(existing) => existing
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(e) => panic!("read existing manifest {manifest:?}: {e}"),
    };
    let before = names.len();
    names.extend(built.iter().cloned());
    let mut contents = names.iter().cloned().collect::<Vec<_>>().join("\n");
    contents.push('\n');
    std::fs::write(&manifest, contents).expect("write manifest");
    info!(
        "manifest {manifest:?}: {} entries ({} new)",
        names.len(),
        names.len() - before
    );

    // Merge fresh entries into the JSON manifest (newest wins per cell); replaced atomically.
    let json_path = out_dir.join(JSON_MANIFEST_FILENAME);
    let mut doc = match std::fs::read_to_string(&json_path) {
        Ok(text) => serde_json::from_str::<ManifestDoc>(&text).unwrap_or_else(|e| {
            panic!("parse existing manifest {json_path:?}: {e}");
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ManifestDoc::default(),
        Err(e) => panic!("read existing manifest {json_path:?}: {e}"),
    };
    let json_before = doc.artifacts.len();
    doc.artifacts.extend(artifacts);
    write_manifest_atomically(&json_path, &doc);
    info!(
        "json manifest {json_path:?}: {} entries ({} new)",
        doc.artifacts.len(),
        doc.artifacts.len() - json_before
    );

    info!(
        "{} failed, {} shards built into {out_dir:?}",
        failed.len(),
        built.len()
    );
}

/// A local mirror of `routers_realtime::region::Manifest` (its source of truth),
/// duplicated because `routers_shard` must not depend on `routers_realtime`.
#[derive(Serialize, Deserialize)]
struct ManifestDoc {
    version: u32,
    #[serde(default)]
    artifacts: BTreeMap<String, ArtifactDoc>,
}

impl Default for ManifestDoc {
    fn default() -> Self {
        ManifestDoc {
            version: 1,
            artifacts: BTreeMap::new(),
        }
    }
}

/// A local mirror of `routers_realtime::region::Artifact` (see [`ManifestDoc`]).
#[derive(Serialize, Deserialize)]
struct ArtifactDoc {
    file: String,
    sha256: String,
    bytes: u64,
    graph: String,
    precision: u8,
    nodes: u64,
    edges: u64,
    built_at: String,
}

/// Serialise `doc` and replace `path` atomically (sibling temp file + rename), so
/// a crashed run leaves the previous manifest intact.
fn write_manifest_atomically(path: &Path, doc: &ManifestDoc) {
    let json = serde_json::to_string_pretty(doc).expect("serialise manifest");
    let tmp = path.with_file_name(format!(
        "{JSON_MANIFEST_FILENAME}.tmp.{}",
        std::process::id()
    ));
    std::fs::write(&tmp, json).unwrap_or_else(|e| panic!("write {tmp:?}: {e}"));
    std::fs::rename(&tmp, path).unwrap_or_else(|e| panic!("rename {tmp:?} -> {path:?}: {e}"));
}

/// The streaming lowercase-hex SHA-256 and byte length of a file (buffered, so a
/// large bundle never sits in memory).
fn artifact_metadata(path: &Path) -> std::io::Result<(String, u64)> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        hasher.update(&buffer[..read]);
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hasher.finalize();
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for &byte in digest.iter() {
        sha256.push(HEX[(byte >> 4) as usize] as char);
        sha256.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok((sha256, bytes))
}

/// The NATS-token-safe rule (`[A-Za-z0-9_-]+`), duplicated from
/// `routers_realtime::protocol::ids::token_safe`.
fn token_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The current wall-clock time, isolated so the `disallowed_methods` allow (the
/// workspace bans `SystemTime::now`, but `web_time` re-exports `std` here) is scoped.
#[allow(clippy::disallowed_methods)]
fn now() -> SystemTime {
    SystemTime::now()
}

/// Format a `SystemTime` as an RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`);
/// a pre-epoch time (not reachable here) formats as the epoch.
fn rfc3339_utc(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let seconds_of_day = secs % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert days since 1970-01-01 to a proleptic-Gregorian `(year, month, day)`
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

// Thin wrapper around the network to allow iterating over the values
struct OsmSource<'a>(&'a OsmNetwork);

impl<'a> ShardSource<OsmEntryId, OsmEdgeMetadata> for OsmSource<'a> {
    fn nodes<'b>(&'b self) -> Box<dyn Iterator<Item = (OsmEntryId, Point)> + 'b> {
        Box::new(self.0.hash.values().map(|n| (n.id, n.position)))
    }

    fn edges<'b>(
        &'b self,
    ) -> Box<dyn Iterator<Item = (OsmEntryId, OsmEntryId, Weight, OsmEdgeMetadata)> + 'b> {
        Box::new(
            self.0
                .graph
                .all_edges()
                .filter_map(|(from, to, (weight, edge_id))| {
                    let meta = self.0.meta.get(&edge_id.index())?.clone();
                    Some((from, to, *weight, meta))
                }),
        )
    }
}

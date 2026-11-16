// `core::io::ErrorKind` is unstable, so `std::io::ErrorKind` is the only stable
// path and `clippy::std_instead_of_core` cannot be followed here.
#![allow(clippy::std_instead_of_core)]

use clap::{Args as ClapArgs, Parser};
use log::{debug, error, info};
use std::path::{Path, PathBuf};
use web_time::{SystemTime, UNIX_EPOCH};

extern crate alloc;
use alloc::collections::BTreeMap;

use rayon::prelude::*;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_shard::{
    Artifact, ArtifactError, GeohashStrategy, MANIFEST_FILENAME, Manifest, ShardedNetwork,
    sha256_hex_of, token_safe,
};

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
        network.num_nodes(),
        network.num_edges()
    );

    let strategy = GeohashStrategy::with_precision(args.precision);

    // Cut straight from the network's columns: each shard is built on its
    // own, so they are cut and written in parallel, and nothing is held but
    // the network and the shards in flight. Spatial indices are not
    // serialised, so none are built.
    let mut carving = ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, _>::carve(
        network.graph(),
        &strategy,
        args.padding,
    );
    let dealt = carving.len();
    if let Some(prefix) = &args.only_prefix {
        carving.retain(|id| id.to_string().starts_with(prefix));
        info!(
            "dealt into {dealt} shards, keeping the {} under prefix {prefix:?}",
            carving.len()
        );
    }
    let total = carving.len();
    info!("writing {total} shards to {out_dir:?}");

    let built_at = rfc3339_utc(now());
    let written = (0..total)
        .into_par_iter()
        .map(|at| {
            let net = carving.shard(at);
            let name = format!("{}.shard.rt", net.owned);
            debug!("[{} / {total}] {net:?}", at + 1);

            let bytes = net.to_cache_bytes().map_err(|e| (name.clone(), e))?;
            std::fs::write(out_dir.join(&name), &bytes)
                .map_err(|e| (name.clone(), e.to_string()))?;

            let artifact = Artifact {
                file: name,
                sha256: sha256_hex_of(&bytes),
                bytes: bytes.len() as u64,
                graph: args.graph_version.clone(),
                precision: args.precision,
                nodes: net.num_nodes() as u64,
                edges: net.num_edges() as u64,
                built_at: built_at.clone(),
            };
            Ok((net.owned.to_string(), artifact))
        })
        .collect::<Vec<Result<_, (String, String)>>>();

    let mut artifacts: BTreeMap<String, Artifact> = BTreeMap::new();
    let mut failed = Vec::new();
    for shard in written {
        match shard {
            Ok((cell, artifact)) => {
                artifacts.insert(cell, artifact);
            }
            Err((name, e)) => {
                error!("failed to write {name}: {e}");
                failed.push(name);
            }
        }
    }
    let built = artifacts.len();

    // This JSON document is the one contract shared by generator and runtime.
    let manifest_path = out_dir.join(MANIFEST_FILENAME);
    let mut manifest = match Manifest::load(&out_dir) {
        Ok(manifest) => manifest,
        Err(ArtifactError::MissingManifest { .. }) => Manifest::new(BTreeMap::new()),
        Err(error) => panic!("load existing manifest {manifest_path:?}: {error}"),
    };
    let before = manifest.artifacts.len();
    manifest.merge(Manifest::new(artifacts));
    write_manifest_atomically(&manifest_path, &manifest);
    info!(
        "manifest {manifest_path:?}: {} entries ({} new)",
        manifest.artifacts.len(),
        manifest.artifacts.len() - before
    );

    info!(
        "{} failed, {built} shards built into {out_dir:?}",
        failed.len()
    );
}

/// Serialise `doc` and replace `path` atomically (sibling temp file + rename), so
/// a crashed run leaves the previous manifest intact.
fn write_manifest_atomically(path: &Path, manifest: &Manifest) {
    let json = serde_json::to_string_pretty(manifest).expect("serialise manifest");
    let tmp = path.with_file_name(format!("{MANIFEST_FILENAME}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, json).unwrap_or_else(|e| panic!("write {tmp:?}: {e}"));
    std::fs::rename(&tmp, path).unwrap_or_else(|e| panic!("rename {tmp:?} -> {path:?}: {e}"));
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

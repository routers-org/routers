use clap::{Args as ClapArgs, Parser};
use geo::Point;
use itertools::Itertools;
use log::{debug, error, info, trace};
use std::{collections::HashSet, path::PathBuf};

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_network::edge::Weight;
use routers_shard::{
    Geohash, GeohashStrategy, Selection, SelectionMode, ShardId, ShardSource, ShardedNetwork,
    ShardingStrategy,
};

const PADDING_DISTANCE: f64 = 1000.0;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The path to the PBF/RT file to load.
    #[command(flatten)]
    file: FileInput,

    /// The precision of the geohash strategy to use.
    #[arg(short, long, env, default_value = "4")]
    precision: u8,

    /// The output directory to write shard files to.
    #[arg(short, long, env = "CARGO_MANIFEST_DIR")]
    output: PathBuf,

    /// The name of the manifest file to write.
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

    let out_dir = args.output.join("../../target/shard_cache");
    std::fs::create_dir_all(&out_dir).expect("create shard_cache dir");

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

    let mut cells: HashSet<Geohash> = HashSet::new();
    for node in network.hash.values() {
        cells.insert(strategy.locate(node.position));
    }

    debug!("contains {} unique geohash cells", cells.len());
    trace!("contains cells={cells:?}");

    let (built, failed): (Vec<_>, Vec<_>) = cells
        .into_iter()
        .map(|cell| {
            sharded_network(&network, strategy.clone(), cell).and_then(|net| {
                let path = out_dir.join(format!("{}.shard.rt", net.owned));
                if let Err(e) = net.save_to_file(&path) {
                    error!("failed to save file: {e}");
                    return Err(e);
                }

                Ok(net.owned)
            })
        })
        .partition_result();

    // Write manifest
    let manifest = out_dir.join(args.manifest_filename);
    let names = built
        .iter()
        .map(|shard| format!("{shard}.shard.rt"))
        .collect::<Vec<_>>()
        .join("\n");

    std::fs::write(&manifest, names).expect("write manifest");

    // Log failed shard reasons
    for (i, failure) in failed.iter().enumerate() {
        error!(
            "[{} / {}] failed to build shard: {failure:?}",
            i + 1,
            failed.len()
        );
    }

    info!(
        "{} failed, {} shards built into {out_dir:?}",
        failed.len(),
        built.len()
    );
}

fn sharded_network<'a, S: ShardId, St: ShardingStrategy<Id = S>>(
    network: &'a OsmNetwork,
    strategy: St,
    cell: S,
) -> Result<ShardedNetwork<OsmEntryId, OsmEdgeMetadata, S>, String> {
    let selection = Selection::new(
        &strategy,
        cell,
        SelectionMode::OwnedAndPadded {
            padding_distance: PADDING_DISTANCE,
        },
    );

    let source = OsmSource(&network);
    ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, S>::from_source(&source, &strategy, &selection)
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

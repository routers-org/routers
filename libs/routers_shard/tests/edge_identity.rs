//! Shard edges keep their source's edge id and direction, and each edge reads
//! the metadata of its own id, even where several edges leave one junction.

use core::num::NonZeroU8;

use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::{DataPlane, Direction, DirectionAwareEdgeId};
use routers_shard::{
    GeohashStrategy, Selection, SelectionMode, ShardId, ShardSource, ShardedNetwork,
    ShardingStrategy, SourceEdge,
};

const JUNCTION: OsmEntryId = OsmEntryId::node(1);
const EAST: OsmEntryId = OsmEntryId::node(2);
const NORTH: OsmEntryId = OsmEntryId::node(3);

const ORIGIN: Point = Point(geo::Coord {
    x: 151.2,
    y: -33.87,
});

/// A two-way way east of a junction and a one-way way north of it, each
/// with its own lane count.
struct Junction;

fn lanes(count: u8) -> OsmEdgeMetadata {
    OsmEdgeMetadata {
        lane_count: NonZeroU8::new(count),
        ..OsmEdgeMetadata::default()
    }
}

impl ShardSource<OsmEntryId, OsmEdgeMetadata> for Junction {
    fn nodes<'a>(&'a self) -> Box<dyn Iterator<Item = (OsmEntryId, Point)> + 'a> {
        Box::new(
            [
                (JUNCTION, ORIGIN),
                (EAST, Point::new(ORIGIN.x() + 0.001, ORIGIN.y())),
                (NORTH, Point::new(ORIGIN.x(), ORIGIN.y() + 0.001)),
            ]
            .into_iter(),
        )
    }

    fn edges<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = SourceEdge<OsmEntryId, OsmEdgeMetadata>> + 'a> {
        let two_way = DirectionAwareEdgeId::new(OsmEntryId::way(10));
        let one_way = DirectionAwareEdgeId::new(OsmEntryId::way(20));
        Box::new(
            [
                (JUNCTION, EAST, (1, two_way.forward()), lanes(1)),
                (EAST, JUNCTION, (1, two_way.backward()), lanes(1)),
                (JUNCTION, NORTH, (1, one_way.forward()), lanes(2)),
            ]
            .into_iter(),
        )
    }
}

fn assert_edge_identity<S: ShardId>(net: &ShardedNetwork<OsmEntryId, OsmEdgeMetadata, S>) {
    let edge = |from: OsmEntryId, to: OsmEntryId| {
        let (_, _, (_, id)) = net
            .edges_outof(from)
            .find(|(_, target, _)| *target == to)
            .expect("edge is in the shard");
        let lanes = net
            .metadata(&id.index())
            .and_then(|meta| meta.lane_count)
            .map(NonZeroU8::get);
        (id.index(), id.direction(), lanes)
    };

    assert_eq!(
        edge(JUNCTION, EAST),
        (OsmEntryId::way(10), Direction::Outgoing, Some(1))
    );
    assert_eq!(
        edge(EAST, JUNCTION),
        (OsmEntryId::way(10), Direction::Incoming, Some(1))
    );
    assert_eq!(
        edge(JUNCTION, NORTH),
        (OsmEntryId::way(20), Direction::Outgoing, Some(2))
    );
}

#[test]
fn from_source_keeps_edge_ids_and_their_metadata() {
    let strategy = GeohashStrategy::with_precision(4);
    let selection = Selection::new(
        &strategy,
        strategy.locate(ORIGIN),
        SelectionMode::OwnedAndPadded {
            padding_distance: 0.0,
        },
    );
    let net = ShardedNetwork::from_source(&Junction, &strategy, &selection).expect("build");
    assert_edge_identity(&net);
}

#[test]
fn partition_keeps_edge_ids_and_their_metadata() {
    let strategy = GeohashStrategy::with_precision(4);
    let shards: Vec<_> = ShardedNetwork::partition(&Junction, &strategy, 0.0).collect();
    assert_eq!(shards.len(), 1, "the junction fits in one cell");
    assert_edge_identity(&shards[0]);
}

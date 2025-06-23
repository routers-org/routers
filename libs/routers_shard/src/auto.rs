use crate::error::ShardError;
use geo::Coord;
use routers_routers_codec::osm::Way;
use scc::HashIndex;
use std::hash::Hash;

/// `Shard<T>
///
/// Stores a quad-tree of inner typed self.
/// The data stored on each shard is generic.
pub struct Shard<T: Hash> {
    // At the bottom-most depth, there will be no children.
    pub children: Box<Option<[Shard<T>; 4]>>,
    pub data: T,
}

pub struct ShardData<T: Hash> {
    // Maps the OSM-PBF Node's as a HashMap to the node itself.
    // As we never modify the underlying content we don't need
    // to lock or grab mutable references to the structure
    //
    // We usually store something like Way's here, but it can
    // be used to store anything, as a generic.
    pub nodes: HashIndex<i64, T>,
}

// An internal representation of a `way`
// We use this to store the respective nodes that
// the way references, *with* the way, to reduce
// lookup times.
#[derive(Clone)]
pub struct WaySide {
    pub(crate) nodes: Vec<Coord<i64>>,
    way_id: i64, // Box<osmpbf::Way<'static>>,
}

impl WaySide {
    fn from_way(way: &Way) -> WaySide {
        WaySide {
            nodes: vec![],
            // nodes: way.node_locations().map(|node| {
            //     Coord {
            //         x: node.nano_lat(),
            //         y: node.nano_lon()
            //     }
            // }).collect(),
            way_id: way.id,
        }
    }
}

impl ShardData<WaySide> {
    fn empty() -> Self {
        ShardData {
            nodes: HashIndex::new(),
        }
    }

    fn insert(&mut self, key: i64, value: WaySide) -> Result<(), (i64, WaySide)> {
        self.nodes.insert(key, value)
    }
}

// impl<T> IntoIterator for Shard<T> {
//     type Item = Shard<T>;
//     type IntoIter = std::vec::IntoIter<Self::Item>;
//
//     fn into_iter(self) -> Self::IntoIter {
//         self.children.iter().flat_map(|map| map.iter().flatten()).into_iter()
//     }
// }

impl Shard<ShardData<WaySide>> {
    pub fn from_file(file_name: &str) -> Result<Self, ShardError> {
        println!("Generating Node Structure from {file_name}\n");
        // let path = std::path::Path::new(file_name);

        let mut data = ShardData::empty();

        // reader.read_ways_and_deps(
        //     |way| {
        //         way.tags().any(|key_value| key_value.0 == "highway")
        //     },
        //     |node| {
        //         match node {
        //             Element::Way(w) => {
        //                 println!("WAYID: {}", w.id());
        //                 if let Err(err) = data.insert(w.id(), WaySide::from_way(w)) {
        //                     println!("Failed to insert ID {}.", err.0);
        //                 }
        //             },
        //             Element::DenseNode(_) => println!("Has dense."),
        //             Element::Node(_) => println!("Has node."),
        //             _ => {}
        //         }
        //     }
        // ).expect("Failed to read.");

        println!("Ingested nodes. Have {} nodes.", data.nodes.len());

        Ok(Shard {
            children: Box::new(None),
            data,
        })
    }
}

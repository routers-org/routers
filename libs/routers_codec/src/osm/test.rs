#![cfg(test)]

use log::error;
use rayon::iter::{ParallelBridge, ParallelIterator};
use std::time::Instant;

use crate::osm::blob::iterator::BlobIterator;
use crate::osm::block::item::BlockItem;
use crate::osm::block::iterator::BlockIterator;

use routers_fixtures::{BADEN_WUERTTEMBERG, DISTRICT_OF_COLUMBIA, fixture_path};

#[test]
fn iterate_blobs_each() {
    let path = fixture_path(BADEN_WUERTTEMBERG);
    let iterator = BlobIterator::new(path.clone());

    let now = Instant::now();

    let total_data_size = iterator.map(|f| {
        f.map(|blob| {
            // println!("Have blob: {}. Type: {}", blob.header.datasize, blob.header.r#type);
            blob.header.datasize
        })
        .reduce(|a, b| a + b)
    });

    match total_data_size {
        Ok(size) => {
            println!("Got Size: {size:?}")
        }
        Err(err) => {
            error!(
                "Failed to load file, {:?}. Got error: {err}",
                path.as_os_str().to_str()
            );
        }
    }

    println!("Time Taken: {}ms", now.elapsed().as_micros() / 1000);
    println!("Test Complete.");
}

#[test_log::test]
fn iterate_blocks_each() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let iterator = BlockIterator::new(path.clone());

    let mut primitive_blocks = 0;
    let mut header_blocks = 0;

    match iterator {
        Ok(iter) => {
            for block in iter {
                match block {
                    BlockItem::HeaderBlock(_) => header_blocks += 1,
                    BlockItem::PrimitiveBlock(_) => primitive_blocks += 1,
                }
            }
        }
        Err(err) => {
            error!(
                "Failed to load file, {:?}. Got error: {err}",
                path.as_os_str().to_str()
            );
        }
    }

    assert_eq!(header_blocks, 1);
    assert_eq!(primitive_blocks, 237);
}

#[test_log::test]
fn parallel_iterate_blocks_each() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);

    let block_iter = BlockIterator::new(path).unwrap();

    let elements = block_iter
        .into_iter()
        .par_bridge()
        .map(|block| match block {
            BlockItem::HeaderBlock(_) => (0, 1),
            BlockItem::PrimitiveBlock(_) => (1, 0),
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));

    assert_eq!(elements, (237, 1));
}

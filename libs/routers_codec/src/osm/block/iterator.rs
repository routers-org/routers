//! Iterates over `BlockItem`s in the file

use crate::osm::BlobItem;
use crate::osm::blob::iterator::BlobIterator;
use crate::osm::block::item::BlockItem;

use alloc::sync::Arc;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::fs::File;
use std::io;
use std::io::{BufReader, Read};
use std::path::PathBuf;

pub struct BlockIterator {
    blobs: Vec<BlobItem>,
    buf: Arc<Vec<u8>>,
    index: usize,
}

impl BlockIterator {
    #[inline]
    pub fn new(path: PathBuf) -> Result<BlockIterator, io::Error> {
        let file = File::open(path)?;

        let mut buf = Vec::new();
        let mut reader = BufReader::new(file);
        reader.read_to_end(&mut buf)?;

        let buf = Arc::new(buf);

        let iter = BlobIterator::with_existing(Arc::clone(&buf))?;
        let blobs = iter.collect::<Vec<_>>();

        Ok(BlockIterator {
            index: 0,
            blobs,
            buf,
        })
    }

    #[inline]
    pub fn par_iter(&mut self) -> impl ParallelIterator<Item = BlockItem> + '_ {
        self.blobs
            .par_iter()
            .filter_map(|blob| BlockItem::from_blob_item(blob, self.buf.as_slice()))
    }
}

impl Iterator for BlockIterator {
    type Item = BlockItem;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let blob = self.blobs.get(self.index)?;
        let block = BlockItem::from_blob_item(blob, self.buf.as_slice());
        self.index += 1;
        block
    }
}

// impl ParallelIterator for BlockIterator {
//     fn next(&mut self) -> Option<Self::Item> {
//
//     }
// }

// impl ParallelIterator for BlockIterator {
//     type Item = BlockItem;
//
//     fn drive_unindexed<C>(self, consumer: C) -> C::Result
//     where
//         C: UnindexedConsumer<BlockItem> + Consumer<BlockItem>,
//     {
//         let (sender, receiver) = channel::unbounded();
//
//         // Spawn a thread to feed items into the channel
//         std::thread::spawn(move || {
//             let mut raw = self.into_iter();
//
//             for item in raw.by_ref() {
//                 if sender.send(item).is_err() {
//                     break;
//                 }
//             }
//         });
//
//         receiver.into_iter().par_bridge().drive_unindexed(consumer)
//     }
// }

// impl<'a> Iterator for BlockIterator {
//     type Item = BlockItem;
//
//     fn next(&mut self) -> Option<Self::Item> {
//         match self.iter.next() {
//             Some(blob) => BlockItem::from_blob_item(&blob),
//             None => None
//         }
//     }
//
//     // #[cfg(not(feature = "mmap"))]
//     // fn next(&mut self) -> Option<Self::Item> {
//     //     let blob_desc = &self.blobs[self.index];
//     //     self.index += 1;
//     //     BlockItem::from_blob_item(blob_desc, &mut self.file)
//     // }
// }

//! The file blob iterator
//! Supports `mmap` reading through the optional feature

use crate::osm::BlobHeader;
use crate::osm::BlockItem;
use crate::osm::blob::item::BlobItem;

use prost::Message;
use std::fs::File;
use std::io;
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::sync::Arc;

const HEADER_LEN_SIZE: usize = 4;

pub struct BlobIterator {
    pub(crate) buf: Arc<Vec<u8>>,

    pub(crate) index: u64,
    offset: u64,
}

impl BlobIterator {
    pub fn new(path: PathBuf) -> Result<BlobIterator, io::Error> {
        let file = File::open(path)?;

        let mut buf = Vec::new(); // vec![0; file.metadata()?.size() as usize];
        let mut reader = BufReader::new(file);
        reader.read_to_end(&mut buf)?;
        let buf = Arc::new(buf);

        Ok(BlobIterator {
            buf,
            offset: 0,
            index: 0,
        })
    }

    pub fn with_existing(buf: Arc<Vec<u8>>) -> Result<BlobIterator, io::Error> {
        // let file = File::open(path)?;

        // let mut buf = Vec::new(); // vec![0; file.metadata()?.size() as usize];
        // let mut reader = BufReader::new(file);
        // reader.read_to_end(&mut buf)?;

        Ok(BlobIterator {
            buf,
            offset: 0,
            index: 0,
        })
    }

    pub fn make_block(&self, blob: &BlobItem) -> Option<BlockItem> {
        BlockItem::from_blob_item(blob, &self.buf)
    }
}

impl BlobIterator {
    fn take_next(&mut self) -> Option<BlobItem> {
        if self.buf.len() < self.offset as usize + HEADER_LEN_SIZE {
            return None;
        }

        let header_len_buffer =
            unsafe { self.buf.as_ptr().add(self.offset as usize) as *const [u8; HEADER_LEN_SIZE] };
        self.offset += HEADER_LEN_SIZE as u64;

        // Translate to i32 (Big Endian)
        let blob_header_length = u32::from_be_bytes(unsafe { *header_len_buffer }) as usize;

        if self.buf.len() < self.offset as usize + blob_header_length {
            return None;
        }

        let blob_header_buffer =
            &self.buf[self.offset as usize..self.offset as usize + blob_header_length];
        self.offset += blob_header_length as u64;

        let start = self.offset;
        let header = BlobHeader::decode(blob_header_buffer).ok()?;
        self.offset += header.datasize as u64;

        let blob = BlobItem::new(start as usize, header)?;
        self.index += 1;

        Some(blob)
    }
}

impl Iterator for BlobIterator {
    type Item = BlobItem;

    fn next(&mut self) -> Option<Self::Item> {
        self.take_next()
    }
}

// #[gat]
// impl LendingIterator for BlobIterator {
//     type Item<'next> where Self: 'next = BlobItem<'next>;
//
//     fn next(self: &mut Self) -> Option<Item<'_, Self>> {
//         self.take_next()
//     }
// }

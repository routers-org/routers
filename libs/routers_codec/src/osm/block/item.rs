//! A block item, used to decode a `BlobItem` into a `BlockItem`,
//! providing distinction for header and primitive elements, as well
//! as decoding fully, to element level.

use bytes::Buf;
use either::Either;
use flate2::read::ZlibDecoder;
use log::warn;
use prost::Message;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::io::Read;

use crate::osm::blob::item::BlobItem;
use crate::osm::element::item::Element;
use crate::osm::element::item::ProcessedElement;
use crate::osm::{Blob, HeaderBlock, PrimitiveBlock, model::blob::Data};

pub enum BlockItem {
    HeaderBlock(HeaderBlock),
    PrimitiveBlock(PrimitiveBlock),
}

impl BlockItem {
    #[inline]
    pub(crate) fn from_blob_item(blob: &BlobItem, buf: &[u8]) -> Option<Self> {
        BlockItem::from_raw(blob, buf)
    }

    #[inline]
    fn from_raw(blob_item: &BlobItem, buf: &[u8]) -> Option<Self> {
        let data = buf.get(blob_item.range.clone())?;
        let blob = Blob::decode(data).ok()?;

        // Convert raw into actual. Handles ZLIB encoding.
        let data = BlockItem::from_blob(blob)?;
        BlockItem::from_data(data.as_slice(), blob_item)
    }

    #[inline]
    fn from_blob(blob: Blob) -> Option<Vec<u8>> {
        if let Some(Data::ZlibData(data)) = blob.data {
            return BlockItem::zlib_decode(data, blob.raw_size.unwrap_or(0) as usize);
        }

        warn!("Type {:?} not yet supported.", blob.data);
        None
    }

    #[inline]
    fn from_data(data: &[u8], blob: &BlobItem) -> Option<Self> {
        match blob.header.r#type.as_str() {
            "OSMData" => Some(BlockItem::PrimitiveBlock(
                PrimitiveBlock::decode(data).ok()?,
            )),
            "OSMHeader" => Some(BlockItem::HeaderBlock(HeaderBlock::decode(data).ok()?)),
            _ => None,
        }
    }

    #[inline]
    fn zlib_decode(data: prost::bytes::Bytes, raw_size: usize) -> Option<Vec<u8>> {
        let mut decoded = vec![0_u8; raw_size];
        ZlibDecoder::new(data.reader())
            .read_exact(&mut decoded)
            .ok()?;

        Some(decoded)
    }

    pub fn r#type(&self) -> &str {
        match self {
            BlockItem::HeaderBlock(_) => "HeaderBlock",
            BlockItem::PrimitiveBlock(_) => "PrimitiveBlock",
        }
    }

    #[inline]
    pub fn raw_element_iter(&self) -> impl Iterator<Item = Element<'_>> {
        match self {
            BlockItem::PrimitiveBlock(primitive) => Either::Left(
                primitive
                    .primitivegroup
                    .iter()
                    .flat_map(Element::from_group),
            ),
            BlockItem::HeaderBlock(_) => Either::Right(std::iter::empty()),
        }
    }

    #[inline]
    pub fn element_iter(&self) -> impl Iterator<Item = ProcessedElement> + '_ {
        match self {
            BlockItem::PrimitiveBlock(primitive) => Either::Left(
                primitive
                    .primitivegroup
                    .iter()
                    .flat_map(Element::from_group)
                    .flat_map(|element| ProcessedElement::from_raw(element, primitive)),
            ),
            BlockItem::HeaderBlock(_) => Either::Right(std::iter::empty()),
        }
    }

    #[inline]
    pub fn raw_par_iter(&mut self) -> impl ParallelIterator<Item = Element<'_>> + '_ {
        match self {
            BlockItem::PrimitiveBlock(primitive) => Either::Left(
                primitive
                    .primitivegroup
                    .par_iter()
                    .flat_map(Element::from_group),
            ),
            BlockItem::HeaderBlock(_) => Either::Right(rayon::iter::empty()),
        }
    }

    #[inline]
    pub fn par_iter(&mut self) -> impl ParallelIterator<Item = ProcessedElement> + '_ {
        match self {
            BlockItem::PrimitiveBlock(primitive) => Either::Left(
                primitive
                    .primitivegroup
                    .par_iter()
                    .flat_map(Element::from_group)
                    .flat_map(|element| ProcessedElement::from_raw(element, primitive)),
            ),
            BlockItem::HeaderBlock(_) => Either::Right(rayon::iter::empty()),
        }
    }
}

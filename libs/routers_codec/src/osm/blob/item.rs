//! Describes the `BlobItem`, which holds the file reference for an `Element`

use crate::osm::BlobHeader;
use core::ops::Range;

pub struct BlobItem {
    pub(crate) range: Range<usize>,
    pub header: BlobHeader,
}

impl BlobItem {
    #[inline]
    pub(crate) fn new(start: usize, header: BlobHeader) -> Option<BlobItem> {
        let end = start + (header.datasize as u64) as usize;

        Some(BlobItem {
            range: start..end,
            header,
        })
    }
}

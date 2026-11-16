pub mod access;

use crate::osm::{Parser, Tags};
pub use access::AccessTag;
use itertools::Itertools;

pub trait Access {
    fn access(&self) -> Vec<AccessTag>;
}

impl Access for Tags<'_> {
    fn access(&self) -> Vec<AccessTag> {
        Vec::<AccessTag>::parse(self)
            .unwrap_or_default()
            .into_iter()
            // Tags come out of a hash map in no fixed order: break ties, so
            // that the same way is the same bytes from one run to the next.
            .sorted_by_key(|v| (v.access, v.restriction.to_string()))
            .collect::<Vec<_>>()
    }
}

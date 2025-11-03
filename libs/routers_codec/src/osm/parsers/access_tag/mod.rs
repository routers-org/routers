pub mod access;

use crate::osm::{Parser, Tags};
pub use access::AccessTag;
use itertools::Itertools;

pub trait Access {
    fn access(&self) -> Vec<AccessTag>;
}

impl Access for Tags {
    fn access(&self) -> Vec<AccessTag> {
        Vec::<AccessTag>::parse(self)
            .unwrap_or_default()
            .into_iter()
            .sorted_by_key(|v| v.access)
            .collect::<Vec<_>>()
    }
}

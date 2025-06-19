pub mod collection;
pub mod limit;
pub mod restriction;
#[cfg(test)]
mod test;

use crate::osm::element::Tags;
use crate::osm::speed_limit::limit::PossiblyConditionalSpeedLimit;
use crate::osm::{OsmTripConfiguration, Parser};

pub use collection::SpeedLimitCollection;
pub use subtypes::SpeedLimitConditions;

pub(super) mod subtypes {
    use crate::osm::primitives::Directionality;
    use std::num::NonZeroU8;

    pub const LANES: &str = "lanes";

    pub const CONDITION_PATTERN: &str = r"\(([^)]+)\)";
    pub const VALUE_PATTERN: &str = r"^\s*(\d+)(?:\s*([^\s(]+))?";

    #[derive(Debug, Default)]
    pub struct SpeedLimitConditions {
        pub directionality: Directionality,
        pub lane: Option<NonZeroU8>,
    }
}

pub trait SpeedLimit {
    fn speed_limit(&self) -> Option<SpeedLimitCollection>;
}

impl SpeedLimit for Tags {
    fn speed_limit(&self) -> Option<SpeedLimitCollection> {
        SpeedLimitCollection::parse(self)
    }
}

pub trait SpeedLimitExt {
    fn relevant_limits(
        &self,
        runtime: &OsmTripConfiguration,
        conditions: SpeedLimitConditions,
    ) -> Vec<PossiblyConditionalSpeedLimit>;
}

use crate::osm::element::{TagString, Tags};
use crate::osm::speed_limit::limit::{SpeedLimitEntry, SpeedLimitVariant};
use crate::osm::speed_limit::subtypes::SpeedLimitConditions;
use crate::osm::speed_limit::{PossiblyConditionalSpeedLimit, SpeedLimitExt};
use crate::osm::{OsmTripConfiguration, Parser};
use std::ops::Deref;

#[derive(Debug, Clone)]
pub struct SpeedLimitCollection(pub(crate) Vec<SpeedLimitEntry>);

impl Deref for SpeedLimitCollection {
    type Target = Vec<SpeedLimitEntry>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl SpeedLimitExt for SpeedLimitCollection {
    fn relevant_limits(
        &self,
        runtime: &OsmTripConfiguration,
        conditions: SpeedLimitConditions,
    ) -> Vec<PossiblyConditionalSpeedLimit> {
        // Must match the conditions if they exist, otherwise blanketed.
        self.0
            .clone()
            .into_iter()
            .filter(|limit| {
                limit
                    .restriction
                    .transport_mode
                    .is_none_or(|mode| mode == runtime.transport_mode)
            })
            .filter(|limit| {
                limit
                    .restriction
                    .directionality
                    .is_none_or(|dir| dir == conditions.directionality)
            })
            .filter_map(|SpeedLimitEntry { limit, .. }| match limit {
                SpeedLimitVariant::Blanket(blanket) => Some(blanket),
                SpeedLimitVariant::PerLane(per_lane) => per_lane
                    .0
                    .get(conditions.lane?.get() as usize)
                    .cloned()
                    .and_then(|x| x),
            })
            .collect::<Vec<_>>()
    }
}

impl Parser for SpeedLimitCollection {
    fn parse(tags: &Tags) -> Option<Self> {
        // Standard structure follows:
        let known_limits = tags
            .iter()
            .filter(|(key, _)| key.starts_with(TagString::MAX_SPEED))
            .filter_map(|(l, v)| SpeedLimitEntry::parse_tag(l, v))
            .collect::<Vec<_>>();

        if known_limits.is_empty() {
            return None;
        }

        Some(Self(known_limits))
    }
}

use serde::Serialize;

use crate::osm::element::TagString;
use crate::osm::primitives::*;
use crate::osm::speed_limit::restriction::{Restriction, RestrictionOptionals};
use crate::osm::speed_limit::subtypes;

/// Defines a speed limit which may contain a conditional element.
/// For example:
///  - `maxspeed=50 @ (Mo-Fr 06:00-22:00)`
///  - `maxspeed:lanes=100 @ (22:00-06:00)|60`
///
/// This represents the individual `number <unit> @ (...)` value.
/// This may be spread across a lane representation in the SpeedLimit
/// structure.
#[derive(Clone, Debug, Serialize)]
pub struct PossiblyConditionalSpeedLimit {
    /// Represents the speed limit on a singular lane.
    ///
    /// See: https://wiki.openstreetmap.org/wiki/Key:lanes
    pub speed: SpeedValue,

    /// The general condition requirements which are separate from
    /// other general conditions. These appear in the post-fix brackets.
    ///
    /// See: https://wiki.openstreetmap.org/wiki/Key:maxspeed:conditional
    pub condition: Option<Condition>,
}

impl PossiblyConditionalSpeedLimit {
    /// Parses the condition on a speed limit value.
    ///
    /// It extracts the condition value and passes it to the `Condition`
    /// structure, attempting to parse the content. None is returned if
    /// this operation fails, or no condition exists on the input.
    fn parse_condition(value: &str) -> Option<Condition> {
        let re = regex::Regex::new(subtypes::CONDITION_PATTERN).ok()?;
        let condition_str = re.captures(value)?.get(1)?.as_str();

        Condition::parse(condition_str).ok()
    }

    /// Parses the speed value (unit and number) on a speed limit value.
    ///
    /// It extracts the speed limit and passes it to the `Speed` structure,
    /// attempting to parse the content inside. None is returned if this
    /// operation fails.
    fn parse_speed(value: &str) -> Option<SpeedValue> {
        let re = regex::Regex::new(subtypes::VALUE_PATTERN).ok()?;
        let captures = re.captures(value)?;

        let (value, unit) = (
            captures
                .get(1)
                .map(|v| v.as_str().to_lowercase())
                .unwrap_or_default(),
            captures
                .get(2)
                .map(|v| v.as_str().to_lowercase())
                .unwrap_or_default(),
        );

        SpeedValue::parse(value, unit)
    }

    /// Parses the speed limit from an individual tag value,
    /// like `50 @ (Mo-Fr 06:00-22:00)`.
    ///
    /// If broken down into individual lanes, this function
    /// can be applied to each individual lane's value.
    ///
    /// For example, this function does not accept `50 @ (...)|50 @ (...)`.
    /// Instead, pass each entry independently.
    pub fn parse(value: &str) -> Option<Self> {
        // Ignore values such as the middle lane in `20||20`,
        // where the lane does not contain an entry.
        if value.is_empty() {
            return None;
        }

        let condition = Self::parse_condition(value);
        let speed = Self::parse_speed(value)?;

        Some(PossiblyConditionalSpeedLimit { condition, speed })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PerLaneSpeedLimit(pub Vec<Option<PossiblyConditionalSpeedLimit>>);

impl PerLaneSpeedLimit {
    pub fn in_kmh(&self) -> Vec<Option<Speed>> {
        self.0
            .iter()
            .map(|lane| lane.as_ref().and_then(|lan| lan.speed.in_kmh()))
            .collect::<Vec<_>>()
    }
}

#[derive(Clone, Debug, Serialize)]
pub enum SpeedLimitVariant {
    /// Applies to every lane within the way, and is
    /// therefore non-dependent on lanes.
    Blanket(PossiblyConditionalSpeedLimit),

    /// Contains an array with size `N` where `N` is the
    /// number of lanes. Each `None` value in the
    /// vector is for lanes in which the limit is not
    /// specified.
    PerLane(PerLaneSpeedLimit),
}

#[derive(Clone, Debug, Serialize)]
pub struct SpeedLimitEntry {
    pub restriction: RestrictionOptionals,
    pub limit: SpeedLimitVariant,
}

impl SpeedLimitEntry {
    pub(crate) fn parse_tag(label: &TagString, value: &TagString) -> Option<Self> {
        let restriction = Restriction::parse(label);

        // Is lane-based (i.e. maxspeed:lanes=50|20|10)
        // Cannot perform an `ends_with(..)` since the
        // "conditional" flag is the subsuming suffix.
        // I.e. maxspeed:lanes:conditional=20 @ ()|10 @ (Mo-Fr 10:00-12:00)
        let limit = if label.contains(subtypes::LANES) {
            let per_lane_limit = value
                .split_terminator("|")
                .map(PossiblyConditionalSpeedLimit::parse)
                .collect::<Vec<_>>();

            SpeedLimitVariant::PerLane(PerLaneSpeedLimit(per_lane_limit))
        } else {
            let as_str = value.as_str();
            let speed_limit = PossiblyConditionalSpeedLimit::parse(as_str)?;
            SpeedLimitVariant::Blanket(speed_limit)
        };

        Some(Self { limit, restriction })
    }
}

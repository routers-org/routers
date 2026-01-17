#![allow(unsafe_code)]

use crate::osm::Parser;
use crate::osm::element::{TagString, Tags};

use crate::osm::speed_limit::SpeedLimitCollection;
use crate::osm::speed_limit::limit::SpeedLimitEntry;
use crate::osm::speed_limit::limit::SpeedLimitVariant::{Blanket, PerLane};

use crate::osm::primitives::condition::{ConditionType, TimeDateCondition};
use crate::osm::primitives::opening_hours::{Time, TimeRange, Weekday, WeekdayRange};
use crate::osm::primitives::*;

use std::collections::HashMap;
use std::num::NonZeroU16;

#[cfg(test)]
fn parse_singular(key: &str, value: &str) -> SpeedLimitEntry {
    let mut tags = HashMap::new();
    tags.insert(TagString::from(key), TagString::from(value));

    let as_tags = Tags::new(tags);
    let limit = SpeedLimitCollection::parse(&as_tags);
    assert!(limit.is_some(), "must parse successfully");

    let limits = limit.unwrap().0;
    assert_eq!(1, limits.len(), "must contain a singular limit");

    limits[0].clone()
}

#[test]
fn test_parsing_speed_limit() {
    let parsed_limit = parse_singular("maxspeed", "50");

    assert!(
        parsed_limit.restriction.transport_mode.is_none(),
        "must not specify a transport mode"
    );
    assert!(
        parsed_limit.restriction.directionality.is_none(),
        "must not specify a directionality"
    );

    matches!(
        parsed_limit.limit,
        Blanket(limit) if limit.speed.in_kmh().is_some_and(|limit| limit.get() == 50)
    );
}

#[test]
fn test_parsing_speed_limit_mph() {
    let parsed_limit = parse_singular("maxspeed", "20 mph");

    assert!(
        parsed_limit.restriction.transport_mode.is_none(),
        "must not specify a transport mode"
    );
    assert!(
        parsed_limit.restriction.directionality.is_none(),
        "must not specify a directionality"
    );

    matches!(
        parsed_limit.limit,
        Blanket(limit) if limit.speed.in_kmh().is_some_and(|limit| limit.get() == 32)
    );
}

#[test]
fn test_parsing_speed_limit_transport_hgv() {
    let parsed_limit = parse_singular("maxspeed:hgv", "20 mph");

    assert!(
        matches!(
            parsed_limit.restriction.transport_mode,
            Some(TransportMode::Hgv)
        ),
        "must specify an HGV transport mode"
    );
    assert!(
        parsed_limit.restriction.directionality.is_none(),
        "must not specify a directionality"
    );

    matches!(
        parsed_limit.limit,
        Blanket(limit) if limit.speed.in_kmh().is_some_and(|limit| limit.get() == 32)
    );
}

#[test]
fn test_parsing_speed_limit_transport_bus_backward() {
    let parsed_limit = parse_singular("maxspeed:bus:backward", "70");

    assert!(
        matches!(
            parsed_limit.restriction.transport_mode,
            Some(TransportMode::Bus)
        ),
        "must specify a bus transport mode"
    );
    assert!(
        matches!(
            parsed_limit.restriction.directionality,
            Some(Directionality::Backward)
        ),
        "must specify a backward directionality"
    );

    matches!(
        parsed_limit.limit,
        Blanket(limit) if limit.speed.in_kmh().is_some_and(|limit| limit.get() == 32)
    );
}

#[test]
fn test_parsing_conditional() {
    let parsed_limit = parse_singular("maxspeed:conditional", "130 @ (19:00-06:00)");

    assert!(
        parsed_limit.restriction.transport_mode.is_none(),
        "must not specify a transport mode"
    );
    assert!(
        parsed_limit.restriction.directionality.is_none(),
        "must not specify a directionality"
    );

    assert!(
        matches!(parsed_limit.limit, Blanket(..)),
        "must be a blanket assignment"
    );
    assert!(matches!(parsed_limit.limit, Blanket(ref limit) if {
        limit.speed
            .in_kmh()
            .is_some_and(|limit| limit.get() == 130)
    }));
    assert!(matches!(parsed_limit.limit, Blanket(ref limit) if {
        let condition_type =  limit.condition.clone().unwrap().condition_type;
        matches!(condition_type, ConditionType::TimeDate(TimeDateCondition {
            opening_hours, ..
        }) if {
            // Overnight ruling: 19:00-06:00
            opening_hours.rules[0].times[0] == TimeRange {
                start: Time {
                    hour: 19,
                    minute: 0,
                },
                end: Time {
                    hour: 6,
                    minute: 0,
                }
            }
        })
    }));
}

#[test]
fn test_parsing_lanes() {
    let parsed_limit = parse_singular("maxspeed:lanes", "100|80|80|80|80|80");

    assert!(
        parsed_limit.restriction.transport_mode.is_none(),
        "must not specify a transport mode"
    );
    assert!(
        parsed_limit.restriction.directionality.is_none(),
        "must not specify a directionality"
    );

    matches!(parsed_limit.limit, PerLane(..));

    if let PerLane(lanes) = parsed_limit.limit {
        let speeds = lanes.in_kmh();

        assert_eq!(speeds.len(), 6, "must contain exactly 6 lanes");
        unsafe {
            assert_eq!(
                speeds.as_slice(),
                &[
                    Some(NonZeroU16::new_unchecked(100)),
                    Some(NonZeroU16::new_unchecked(80)),
                    Some(NonZeroU16::new_unchecked(80)),
                    Some(NonZeroU16::new_unchecked(80)),
                    Some(NonZeroU16::new_unchecked(80)),
                    Some(NonZeroU16::new_unchecked(80))
                ],
                "must parse limits correctly"
            );
        }
    }
}

#[test]
fn test_parsing_lanes_with_missing() {
    let parsed_limit = parse_singular("maxspeed:lanes", "|50");

    matches!(parsed_limit.limit, PerLane(..));

    if let PerLane(lanes) = parsed_limit.limit {
        let speeds = lanes.in_kmh();

        assert_eq!(speeds.len(), 2, "must contain exactly 2 lanes");
        unsafe {
            assert_eq!(
                speeds.as_slice(),
                &[None, Some(NonZeroU16::new_unchecked(50))],
                "must parse limits correctly"
            );
        }
    }
}

#[test]
fn test_parsing_lanes_mph() {
    let parsed_limit = parse_singular("maxspeed:lanes", "65 mph|65 mph|65 mph|25 mph");

    matches!(parsed_limit.limit, PerLane(..));

    if let PerLane(lanes) = parsed_limit.limit {
        let speeds = lanes.in_kmh();

        assert_eq!(speeds.len(), 4, "must contain exactly 4 lanes");
        unsafe {
            assert_eq!(
                speeds.as_slice(),
                &[
                    Some(NonZeroU16::new_unchecked(104)),
                    Some(NonZeroU16::new_unchecked(104)),
                    Some(NonZeroU16::new_unchecked(104)),
                    Some(NonZeroU16::new_unchecked(40))
                ],
                "must parse limits correctly"
            );
        }
    }
}

#[test]
fn test_parsing_lanes_conditional() {
    let parsed_limit = parse_singular(
        "maxspeed:lanes:conditional",
        "100 @ (22:00-06:00)|40 @ (Mo-Fr 07:00-9:00,16:00-20:00)|60",
    );

    matches!(parsed_limit.limit, PerLane(..));

    if let PerLane(lanes) = parsed_limit.limit {
        let speeds = lanes.in_kmh();

        assert_eq!(speeds.len(), 3, "must contain exactly 3 lanes");
        unsafe {
            assert_eq!(
                speeds.as_slice(),
                &[
                    Some(NonZeroU16::new_unchecked(100)),
                    Some(NonZeroU16::new_unchecked(40)),
                    Some(NonZeroU16::new_unchecked(60))
                ],
                "must parse limits correctly"
            );
        }

        // Expecting 22:00-06:00 condition
        let lane_one = lanes.0[0].as_ref().unwrap();
        let lane_one_condition = lane_one.condition.as_ref();

        assert!(lane_one_condition.is_some(), "must specify a condition");
        assert!(matches!(
            lane_one_condition.unwrap().clone().condition_type,
            ConditionType::TimeDate(date) if {
                date.opening_hours.rules[0].times[0] == TimeRange {
                    start: Time { hour: 22, minute: 0 },
                    end: Time { hour: 6, minute: 0}
                }
            }
        ));

        // Expecting Mo-Fr 07:00-9:00,16:00-20:00 condition
        let lane_two = lanes.0[1].as_ref().unwrap();
        let lane_two_condition = lane_two.condition.as_ref();

        assert!(lane_two_condition.is_some(), "must specify a condition");
        assert!(matches!(
            lane_two_condition.unwrap().clone().condition_type,
            ConditionType::TimeDate(date) if {
                let rule =  date.opening_hours.rules[0].clone();

                rule.times == vec![
                    TimeRange {
                        start: Time { hour: 7, minute: 0 },
                        end: Time { hour: 9, minute: 0 },
                    },
                    TimeRange {
                        start: Time { hour: 16, minute: 0},
                        end: Time { hour: 20, minute: 0},
                    }
                ] && rule.weekdays == Some(WeekdayRange::Range(Weekday::Monday, Weekday::Friday))
            }
        ));

        // Expecting no condition
        let lane_three = (lanes.0)[2].as_ref().unwrap();
        let lane_three_condition = lane_three.condition.as_ref();

        assert!(
            lane_three_condition.is_none(),
            "must not specify a condition"
        );
    }
}

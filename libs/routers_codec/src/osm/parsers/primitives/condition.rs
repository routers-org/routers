use crate::osm::primitives::opening_hours::{OpeningHours, OpeningHoursParser};
use std::fmt;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use strum::{Display, EnumIter, EnumString};

/// Represents a complete conditional restriction condition
/// Examples: "Tu-Fr 00:00-24:00", "winter", "snow", "weight < 7.5"
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub condition_type: ConditionType,
}

/// Main condition types as defined in OSM conditional restrictions
#[derive(Debug, Clone, PartialEq)]
pub enum ConditionType {
    /// Time and date conditions using opening hours syntax
    /// Examples: "Mo-Fr 07:00-19:00", "Tu-Fr 00:00-24:00", "sunrise-sunset"
    TimeDate(TimeDateCondition),

    /// Seasonal conditions for time of year
    /// Examples: "winter", "summer"
    Season(SeasonCondition),

    /// Road surface conditions
    /// Examples: "wet", "snow", "dry"
    RoadCondition(RoadCondition),

    /// Vehicle property restrictions with comparison operators
    /// Examples: "weight<7.5", "length>5", "height<=3.5"
    VehicleProperty(VehiclePropertyCondition),

    /// Vehicle usage conditions
    /// Examples: "occupants>1", "hazmat"
    VehicleUsage(VehicleUsageCondition),

    /// User group restrictions
    /// Examples: "doctor", "disabled", "emergency", "female"
    UserGroup(UserGroupCondition),

    /// Purpose of access conditions
    /// Examples: "destination", "delivery", "customers"
    Purpose(PurposeCondition),

    /// Stay duration conditions
    /// Examples: "stay < 2 hours", "stay >= 30 minutes"
    StayDuration(StayDurationCondition),

    /// Combined conditions using AND operator
    /// Examples: "destination @ Sa-Su AND weight>7"
    Combined(CombinedCondition),

    /// Raw/unparsed condition for complex cases
    Raw(String),
}

/// Time and date conditions using opening hours syntax
#[derive(Debug, Clone, PartialEq)]
pub struct TimeDateCondition {
    /// Raw opening hours string
    /// Examples: "Mo-Fr 07:00-19:00", "sunrise-sunset", "Jan-Mar"
    pub opening_hours: OpeningHours,
    /// Optional comment in local language
    /// Example: "bij grote verkeersdrukte"
    pub comment: Option<String>,
}

/// Seasonal time restrictions
#[derive(Debug, Clone, Copy, PartialEq, Display, EnumString, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum SeasonCondition {
    /// Winter season (dates vary by location/year)
    Winter,
    /// Summer season (dates vary by location/year)
    Summer,
    /// Spring season
    Spring,
    /// Autumn/Fall season
    Autumn,
}

/// Road surface and weather conditions
#[derive(Debug, Clone, PartialEq, Display, EnumString, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum RoadCondition {
    /// Wet road surface
    Wet,
    /// Dry road surface
    Dry,
    /// Snow on road
    Snow,
    /// Ice on road
    Ice,
    /// Rain weather condition
    Rain,
    /// Fog weather condition
    Fog,
}

/// Vehicle property conditions with comparison operators
#[derive(Debug, Clone, PartialEq)]
pub struct VehiclePropertyCondition {
    pub property: VehicleProperty,
    pub operator: ComparisonOperator,
    pub value: f64,
    pub unit: Option<String>,
}

/// Vehicle properties that can be restricted
#[derive(Debug, Clone, PartialEq, Display, EnumString, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum VehicleProperty {
    /// Vehicle weight in tonnes
    Weight,
    /// Axle load in tonnes
    Axleload,
    /// Vehicle length in meters
    Length,
    /// Vehicle width in meters
    Width,
    /// Vehicle height in meters
    Height,
    /// Number of wheels
    Wheels,
    /// Ship draught in meters
    Draught,
}

/// Comparison operators for vehicle properties
#[derive(Debug, Clone, PartialEq, Display)]
pub enum ComparisonOperator {
    #[strum(serialize = "<")]
    LessThan,
    #[strum(serialize = ">")]
    GreaterThan,
    #[strum(serialize = "=")]
    Equal,
    #[strum(serialize = "<=")]
    LessThanOrEqual,
    #[strum(serialize = ">=")]
    GreaterThanOrEqual,
}

/// Vehicle usage conditions
#[derive(Debug, Clone, PartialEq)]
pub enum VehicleUsageCondition {
    /// Number of occupants with comparison
    /// Example: "occupants>1" for HOV lanes
    Occupants {
        operator: ComparisonOperator,
        count: u32,
    },
    /// Vehicle carrying hazardous materials
    Hazmat,
    /// Vehicle carrying specific load type
    Load(String),
}

/// User group conditions for access restrictions
#[derive(Debug, Clone, PartialEq, Display, EnumString, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum UserGroupCondition {
    /// Medical doctors
    Doctor,
    /// Disabled persons
    Disabled,
    /// Emergency services
    Emergency,
    /// Female users (for specific cultural contexts)
    Female,
    /// Residents of the area
    Residents,
    /// Permit holders
    Permit,
    /// Staff/employees
    Staff,
    /// Customers of businesses
    Customers,
}

/// Purpose of access conditions
#[derive(Debug, Clone, PartialEq, Display, EnumString, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum PurposeCondition {
    /// Destination traffic only
    Destination,
    /// Delivery vehicles
    Delivery,
    /// Customer access
    Customers,
    /// Forestry vehicles
    Forestry,
    /// Agricultural vehicles
    Agricultural,
    /// Private access
    Private,
    /// Permit required
    Permit,
}

/// Stay duration conditions
#[derive(Debug, Clone, PartialEq)]
pub struct StayDurationCondition {
    pub operator: ComparisonOperator,
    pub duration: Duration,
}

/// Duration representation
#[derive(Debug, Clone, PartialEq)]
pub struct Duration {
    pub value: u32,
    pub unit: DurationUnit,
}

/// Duration units
#[derive(Debug, Clone, PartialEq, Display, EnumString, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum DurationUnit {
    Minutes,
    Hours,
    Days,
}

/// Combined conditions using logical operators
#[derive(Debug, Clone, PartialEq)]
pub struct CombinedCondition {
    pub left: Box<ConditionType>,
    pub operator: LogicalOperator,
    pub right: Box<ConditionType>,
}

/// Logical operators for combining conditions
#[derive(Debug, Clone, PartialEq, Display, EnumString)]
#[strum(serialize_all = "UPPERCASE")]
pub enum LogicalOperator {
    And,
    Or,
}

impl FromStr for ComparisonOperator {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "<" => Ok(ComparisonOperator::LessThan),
            ">" => Ok(ComparisonOperator::GreaterThan),
            "=" => Ok(ComparisonOperator::Equal),
            "<=" => Ok(ComparisonOperator::LessThanOrEqual),
            ">=" => Ok(ComparisonOperator::GreaterThanOrEqual),
            _ => Err(format!("Unknown comparison operator: {}", s)),
        }
    }
}

impl Display for Condition {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.condition_type {
            ConditionType::TimeDate(td) => {
                if let Some(comment) = &td.comment {
                    write!(f, "{} \"{}\"", td.opening_hours, comment)
                } else {
                    write!(f, "{}", td.opening_hours)
                }
            }
            ConditionType::Season(season) => write!(f, "{}", season),
            ConditionType::RoadCondition(road) => write!(f, "{}", road),
            ConditionType::VehicleProperty(vp) => {
                let unit_str = vp.unit.as_deref().unwrap_or("");
                write!(f, "{}{}{}{}", vp.property, vp.operator, vp.value, unit_str)
            }
            ConditionType::VehicleUsage(vu) => match vu {
                VehicleUsageCondition::Occupants { operator, count } => {
                    write!(f, "occupants{}{}", operator, count)
                }
                VehicleUsageCondition::Hazmat => write!(f, "hazmat"),
                VehicleUsageCondition::Load(load) => write!(f, "{}", load),
            },
            ConditionType::UserGroup(ug) => write!(f, "{}", ug),
            ConditionType::Purpose(purpose) => write!(f, "{}", purpose),
            ConditionType::StayDuration(sd) => {
                write!(
                    f,
                    "stay {} {} {}",
                    sd.operator, sd.duration.value, sd.duration.unit
                )
            }
            ConditionType::Combined(combined) => {
                write!(
                    f,
                    "{} {} {}",
                    Condition {
                        condition_type: *combined.left.clone()
                    },
                    combined.operator,
                    Condition {
                        condition_type: *combined.right.clone()
                    }
                )
            }
            ConditionType::Raw(raw) => write!(f, "{}", raw),
        }
    }
}

impl Condition {
    /// Parse a condition string into a Condition struct
    ///
    /// # Arguments
    /// * `condition_str` - The condition string to parse
    /// * `context` - Optional context from other OSM tags for disambiguation
    ///
    /// # Examples
    /// ```
    /// use routers_routers_codec::osm::primitives::Condition;
    ///
    /// let condition = Condition::parse("Tu-Fr 00:00-24:00")?;
    /// let condition = Condition::parse("weight < 7.5")?;
    /// let condition = Condition::parse("winter")?;
    /// ```
    pub fn parse(condition_str: &str) -> Result<Self, ParseError> {
        let trimmed = condition_str.trim();

        // Handle parentheses
        let cleaned = if trimmed.starts_with('(') && trimmed.ends_with(')') {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        };

        // Check for combined conditions (AND/OR)
        if let Ok(combined) = Self::parse_combined_condition(cleaned) {
            return Ok(Condition {
                condition_type: combined,
            });
        }

        // Try to parse as specific condition types
        if let Ok(time_date) = Self::parse_time_date(cleaned) {
            return Ok(Condition {
                condition_type: ConditionType::TimeDate(time_date),
            });
        }

        if let Ok(season) = Self::parse_season(cleaned) {
            return Ok(Condition {
                condition_type: ConditionType::Season(season),
            });
        }

        if let Ok(road_condition) = Self::parse_road_condition(cleaned) {
            return Ok(Condition {
                condition_type: ConditionType::RoadCondition(road_condition),
            });
        }

        if let Ok(vehicle_prop) = Self::parse_vehicle_property(cleaned) {
            return Ok(Condition {
                condition_type: ConditionType::VehicleProperty(vehicle_prop),
            });
        }

        if let Ok(vehicle_usage) = Self::parse_vehicle_usage(cleaned) {
            return Ok(Condition {
                condition_type: ConditionType::VehicleUsage(vehicle_usage),
            });
        }

        if let Ok(user_group) = Self::parse_user_group(cleaned) {
            return Ok(Condition {
                condition_type: ConditionType::UserGroup(user_group),
            });
        }

        if let Ok(purpose) = Self::parse_purpose(cleaned) {
            return Ok(Condition {
                condition_type: ConditionType::Purpose(purpose),
            });
        }

        if let Ok(stay_duration) = Self::parse_stay_duration(cleaned) {
            return Ok(Condition {
                condition_type: ConditionType::StayDuration(stay_duration),
            });
        }

        // If no specific parser matches, store as raw condition
        Ok(Condition {
            condition_type: ConditionType::Raw(cleaned.to_string()),
        })
    }

    // Private parsing methods

    fn parse_combined_condition(s: &str) -> Result<ConditionType, ParseError> {
        // Look for AND/OR operators (case insensitive)
        let s_upper = s.to_uppercase();

        if let Some(and_pos) = s_upper.find(" AND ") {
            let left_str = &s[..and_pos].trim();
            let right_str = &s[and_pos + 5..].trim();

            let left_condition = Self::parse(left_str)?.condition_type;
            let right_condition = Self::parse(right_str)?.condition_type;

            return Ok(ConditionType::Combined(CombinedCondition {
                left: Box::new(left_condition),
                operator: LogicalOperator::And,
                right: Box::new(right_condition),
            }));
        }

        if let Some(or_pos) = s_upper.find(" OR ") {
            let left_str = &s[..or_pos].trim();
            let right_str = &s[or_pos + 4..].trim();

            let left_condition = Self::parse(left_str)?.condition_type;
            let right_condition = Self::parse(right_str)?.condition_type;

            return Ok(ConditionType::Combined(CombinedCondition {
                left: Box::new(left_condition),
                operator: LogicalOperator::Or,
                right: Box::new(right_condition),
            }));
        }

        Err(ParseError::NotCombinedCondition)
    }

    fn parse_time_date(s: &str) -> Result<TimeDateCondition, ParseError> {
        let (comment, hours) = if let Some(quote_start) = s.find('"') {
            let opening_hours = s[..quote_start].trim().to_string();
            let comment_end = s.rfind('"').unwrap_or(s.len());
            let comment = s[quote_start + 1..comment_end].to_string();
            (Some(comment), opening_hours)
        } else {
            (None, s.to_string())
        };

        Ok(TimeDateCondition {
            comment,
            opening_hours: OpeningHoursParser::parse(&hours)
                .map_err(|_| ParseError::NotTimeDate)?,
        })
    }

    fn parse_season(s: &str) -> Result<SeasonCondition, ParseError> {
        SeasonCondition::from_str(s).map_err(|_| ParseError::NotSeason)
    }

    fn parse_road_condition(s: &str) -> Result<RoadCondition, ParseError> {
        RoadCondition::from_str(s).map_err(|_| ParseError::NotRoadCondition)
    }

    fn parse_vehicle_property(s: &str) -> Result<VehiclePropertyCondition, ParseError> {
        // Look for comparison operators
        let operators = ["<=", ">=", "<", ">", "="];

        for op_str in &operators {
            if let Some(op_pos) = s.find(op_str) {
                let property_str = s[..op_pos].trim();
                let value_str = s[op_pos + op_str.len()..].trim();

                let property = VehicleProperty::from_str(property_str)
                    .map_err(|_| ParseError::UnknownVehicleProperty)?;
                let operator = ComparisonOperator::from_str(op_str)
                    .map_err(|_| ParseError::UnknownOperator)?;

                // Parse value and optional unit
                let (value, unit) = Self::parse_value_with_unit(value_str)?;

                return Ok(VehiclePropertyCondition {
                    property,
                    operator,
                    value,
                    unit,
                });
            }
        }

        Err(ParseError::NotVehicleProperty)
    }

    fn parse_vehicle_usage(s: &str) -> Result<VehicleUsageCondition, ParseError> {
        if s == "hazmat" {
            return Ok(VehicleUsageCondition::Hazmat);
        }

        // Check for occupants condition
        if let Some(rest) = s.strip_prefix("occupants") {
            let operators = ["<=", ">=", "<", ">", "="];

            for op_str in &operators {
                if let Some(op_pos) = rest.find(op_str) {
                    let operator = ComparisonOperator::from_str(op_str)
                        .map_err(|_| ParseError::UnknownOperator)?;
                    let count_str = rest[op_pos + op_str.len()..].trim();
                    let count = count_str
                        .parse::<u32>()
                        .map_err(|_| ParseError::InvalidNumber)?;

                    return Ok(VehicleUsageCondition::Occupants { operator, count });
                }
            }
        }

        // Treat as generic load condition
        Ok(VehicleUsageCondition::Load(s.to_string()))
    }

    fn parse_user_group(s: &str) -> Result<UserGroupCondition, ParseError> {
        UserGroupCondition::from_str(s).map_err(|_| ParseError::NotUserGroup)
    }

    fn parse_purpose(s: &str) -> Result<PurposeCondition, ParseError> {
        PurposeCondition::from_str(s).map_err(|_| ParseError::NotPurpose)
    }

    fn parse_stay_duration(s: &str) -> Result<StayDurationCondition, ParseError> {
        if !s.starts_with("stay") {
            return Err(ParseError::NotStayDuration);
        }

        let rest = s[4..].trim(); // Skip "stay"
        let operators = ["<=", ">=", "<", ">", "="];

        for op_str in &operators {
            if let Some(op_pos) = rest.find(op_str) {
                let operator = ComparisonOperator::from_str(op_str)
                    .map_err(|_| ParseError::UnknownOperator)?;
                let duration_str = rest[op_pos + op_str.len()..].trim();
                let duration = Self::parse_duration(duration_str)?;

                return Ok(StayDurationCondition { operator, duration });
            }
        }

        Err(ParseError::NotStayDuration)
    }

    fn parse_duration(s: &str) -> Result<Duration, ParseError> {
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() != 2 {
            return Err(ParseError::InvalidDuration);
        }

        let value = parts[0]
            .parse::<u32>()
            .map_err(|_| ParseError::InvalidNumber)?;
        let unit_str = parts[1].to_lowercase();

        let unit = match unit_str.as_str() {
            "minute" | "minutes" => DurationUnit::Minutes,
            "hour" | "hours" => DurationUnit::Hours,
            "day" | "days" => DurationUnit::Days,
            _ => return Err(ParseError::InvalidDurationUnit),
        };

        Ok(Duration { value, unit })
    }

    fn parse_value_with_unit(s: &str) -> Result<(f64, Option<String>), ParseError> {
        // Try to parse as pure number first
        if let Ok(value) = s.parse::<f64>() {
            return Ok((value, None));
        }

        // Look for number followed by unit
        let mut number_end = 0;
        for (i, c) in s.chars().enumerate() {
            if c.is_numeric() || c == '.' {
                number_end = i + 1;
            } else {
                break;
            }
        }

        if number_end > 0 {
            let number_str = &s[..number_end];
            let unit_str = &s[number_end..].trim();

            let value = number_str
                .parse::<f64>()
                .map_err(|_| ParseError::InvalidNumber)?;
            let unit = if unit_str.is_empty() {
                None
            } else {
                Some(unit_str.to_string())
            };

            Ok((value, unit))
        } else {
            Err(ParseError::InvalidNumber)
        }
    }
}

/// Errors that can occur during parsing
#[derive(Debug, PartialEq)]
pub enum ParseError {
    NotTimeDate,
    NotSeason,
    NotRoadCondition,
    NotVehicleProperty,
    NotVehicleUsage,
    NotUserGroup,
    NotPurpose,
    NotStayDuration,
    NotCombinedCondition,
    UnknownVehicleProperty,
    UnknownOperator,
    InvalidNumber,
    InvalidDuration,
    InvalidDurationUnit,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::NotTimeDate => write!(f, "Not a time/date condition"),
            ParseError::NotSeason => write!(f, "Not a season condition"),
            ParseError::NotRoadCondition => write!(f, "Not a road condition"),
            ParseError::NotVehicleProperty => write!(f, "Not a vehicle property condition"),
            ParseError::NotVehicleUsage => write!(f, "Not a vehicle usage condition"),
            ParseError::NotUserGroup => write!(f, "Not a user group condition"),
            ParseError::NotPurpose => write!(f, "Not a purpose condition"),
            ParseError::NotStayDuration => write!(f, "Not a stay duration condition"),
            ParseError::NotCombinedCondition => write!(f, "Not a combined condition"),
            ParseError::UnknownVehicleProperty => write!(f, "Unknown vehicle property"),
            ParseError::UnknownOperator => write!(f, "Unknown comparison operator"),
            ParseError::InvalidNumber => write!(f, "Invalid number format"),
            ParseError::InvalidDuration => write!(f, "Invalid duration format"),
            ParseError::InvalidDurationUnit => write!(f, "Invalid duration unit"),
        }
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osm::primitives::opening_hours::{
        OpeningRule, Time, TimeRange, Weekday, WeekdayRange,
    };

    #[test]
    fn test_parse_time_date() {
        let condition = Condition::parse("Tu-Fr 00:00-24:00").unwrap();
        if let ConditionType::TimeDate(td) = condition.condition_type {
            // "Tu-Fr 00:00-24:00"
            assert_eq!(
                td.opening_hours.rules[0],
                OpeningRule {
                    weekdays: Some(WeekdayRange::Range(Weekday::Tuesday, Weekday::Friday)),
                    times: vec![TimeRange {
                        start: Time { hour: 0, minute: 0 },
                        end: Time {
                            hour: 24,
                            minute: 0
                        }
                    }],
                    closed: false
                }
            );
            assert_eq!(td.comment, None);
        } else {
            panic!("Expected TimeDate condition");
        }
    }

    #[test]
    fn test_parse_season() {
        let condition = Condition::parse("winter").unwrap();
        if let ConditionType::Season(season) = condition.condition_type {
            assert_eq!(season, SeasonCondition::Winter);
        } else {
            panic!("Expected Season condition");
        }
    }

    #[test]
    fn test_parse_vehicle_property() {
        let condition = Condition::parse("weight < 7.5").unwrap();
        if let ConditionType::VehicleProperty(vp) = condition.condition_type {
            assert_eq!(vp.property, VehicleProperty::Weight);
            assert_eq!(vp.operator, ComparisonOperator::LessThan);
            assert_eq!(vp.value, 7.5);
        } else {
            panic!("Expected VehicleProperty condition");
        }
    }

    #[test]
    fn test_parse_road_condition() {
        let condition = Condition::parse("snow").unwrap();
        if let ConditionType::RoadCondition(road) = condition.condition_type {
            assert_eq!(road, RoadCondition::Snow);
        } else {
            panic!("Expected RoadCondition");
        }
    }

    #[test]
    fn test_to_string_roundtrip() {
        let original = "weight < 7.5";
        let condition = Condition::parse(original).unwrap();
        let regenerated = condition.to_string();
        assert_eq!(regenerated, "weight<7.5"); // Note: spaces might be normalized
    }
}

use either::{Left, Right};
use itertools::Itertools;
use std::fmt;
use std::fmt::{Display, Formatter};
use strum::{Display, EnumString};

#[derive(Debug, Clone, PartialEq, Display, EnumString)]
pub enum Weekday {
    #[strum(serialize = "Mo")]
    Monday,
    #[strum(serialize = "Tu")]
    Tuesday,
    #[strum(serialize = "We")]
    Wednesday,
    #[strum(serialize = "Th")]
    Thursday,
    #[strum(serialize = "Fr")]
    Friday,
    #[strum(serialize = "Sa")]
    Saturday,
    #[strum(serialize = "Su")]
    Sunday,
}

impl Weekday {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "mo" | "monday" => Some(Weekday::Monday),
            "tu" | "tuesday" => Some(Weekday::Tuesday),
            "we" | "wednesday" => Some(Weekday::Wednesday),
            "th" | "thursday" => Some(Weekday::Thursday),
            "fr" | "friday" => Some(Weekday::Friday),
            "sa" | "saturday" => Some(Weekday::Saturday),
            "su" | "sunday" => Some(Weekday::Sunday),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Time {
    pub hour: u8,
    pub minute: u8,
}

impl Time {
    fn new(hour: u8, minute: u8) -> Result<Self, String> {
        if hour > 24 || minute > 59 {
            Err("Invalid time".to_string())
        } else {
            Ok(Time { hour, minute })
        }
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimeRange {
    pub start: Time,
    pub end: Time,
}

impl Display for TimeRange {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimeOfWeek {
    time: Time,
    weekday: Weekday,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WeekdayRange {
    Single(Weekday),
    Range(Weekday, Weekday),
    List(Vec<Weekday>),
}

impl Display for WeekdayRange {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            WeekdayRange::Single(weekday) => {
                write!(f, "{weekday}")
            }
            WeekdayRange::Range(start, end) => {
                write!(f, "{start}-{end}")
            }
            WeekdayRange::List(weekdays) => {
                write!(
                    f,
                    "{}",
                    weekdays.iter().map(|weekday| weekday.to_string()).join(",")
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpeningRule {
    pub weekdays: Option<WeekdayRange>,
    pub times: Vec<TimeRange>,
    pub closed: bool,
}

impl Display for OpeningRule {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(weekday) = &self.weekdays {
            write!(f, "{weekday}")?;
        }

        let times = self.times.iter().map(|s| s.to_string()).join(",");
        write!(f, "{times}")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpeningHours {
    pub rules: Vec<OpeningRule>,
}

impl Display for OpeningHours {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            self.rules.iter().map(|rule| rule.to_string()).join(";")
        )
    }
}

pub struct OpeningHoursParser;

impl OpeningHoursParser {
    pub fn parse(input: &str) -> Result<OpeningHours, String> {
        let input = input.trim();

        // Handle special cases
        if input.eq_ignore_ascii_case("24/7") {
            return Ok(OpeningHours {
                rules: vec![OpeningRule {
                    weekdays: None,
                    times: vec![TimeRange {
                        start: Time::new(0, 0)?,
                        end: Time::new(23, 59)?,
                    }],
                    closed: false,
                }],
            });
        }

        let mut rules = Vec::new();
        let rule_parts: Vec<&str> = input.split(';').collect();

        for rule_part in rule_parts {
            let rule = OpeningHoursParser::parse_rule(rule_part.trim())?;
            rules.push(rule);
        }

        if rules.is_empty() {
            return Err("No rules".to_string());
        }

        Ok(OpeningHours { rules })
    }

    fn parse_rule(rule: &str) -> Result<OpeningRule, String> {
        let rule = rule.trim();

        // Check if it's a closed rule
        if rule.eq_ignore_ascii_case("closed") || rule.eq_ignore_ascii_case("off") {
            return Ok(OpeningRule {
                weekdays: None,
                times: Vec::new(),
                closed: true,
            });
        }

        // Split by space to separate weekdays from times
        let parts: Vec<&str> = rule.split_whitespace().collect();

        if parts.is_empty() {
            return Err("Empty rule".to_string());
        }

        let (time_parts, weekday_parts): (Vec<_>, Vec<_>) = parts
            .iter()
            .scan(false, |parsing_times, part| {
                *parsing_times |= OpeningHoursParser::looks_like_time(part);
                Some((part, *parsing_times))
            })
            .partition_map(|(part, is_time)| if is_time { Left(*part) } else { Right(*part) });

        let weekdays = weekday_parts
            .into_iter()
            .map(OpeningHoursParser::parse_weekday_range)
            .find_map(|val| val.ok());

        // If no weekdays specified, apply to all days
        let times = if time_parts.is_empty() {
            Vec::new()
        } else {
            OpeningHoursParser::parse_time_ranges(&time_parts.join(" "))?
        };

        if weekdays.is_none() && times.is_empty() {
            return Err("No applicable values parsed".to_string());
        }

        Ok(OpeningRule {
            weekdays,
            times,
            closed: false,
        })
    }

    fn looks_like_time(s: &str) -> bool {
        s.contains(':') && s.len() >= 3
    }

    fn parse_weekday_range(input: &str) -> Result<WeekdayRange, String> {
        if input.contains('-') {
            let parts: Vec<&str> = input.split('-').collect();
            if parts.len() != 2 {
                return Err("Invalid weekday range".to_string());
            }
            let start = Weekday::from_str(parts[0]).ok_or("Invalid start weekday")?;
            let end = Weekday::from_str(parts[1]).ok_or("Invalid end weekday")?;
            Ok(WeekdayRange::Range(start, end))
        } else if input.contains(',') {
            let parts: Vec<&str> = input.split(',').collect();
            let mut weekdays = Vec::new();
            for part in parts {
                let weekday = Weekday::from_str(part.trim()).ok_or("Invalid weekday in list")?;
                weekdays.push(weekday);
            }
            Ok(WeekdayRange::List(weekdays))
        } else {
            let weekday = Weekday::from_str(input).ok_or("Invalid weekday")?;
            Ok(WeekdayRange::Single(weekday))
        }
    }

    fn parse_time_ranges(input: &str) -> Result<Vec<TimeRange>, String> {
        let mut ranges = Vec::new();

        // Split by comma for multiple time ranges
        let range_parts: Vec<&str> = input.split(',').collect();

        for range_part in range_parts {
            let range_part = range_part.trim();

            if range_part.contains('-') {
                let parts: Vec<&str> = range_part.split('-').collect();
                if parts.len() != 2 {
                    return Err("Invalid time range format".to_string());
                }

                let start_time = OpeningHoursParser::parse_time(parts[0].trim())?;
                let end_time = OpeningHoursParser::parse_time(parts[1].trim())?;

                ranges.push(TimeRange {
                    start: start_time,
                    end: end_time,
                });
            } else {
                // Single time point - treat as start time with end time one hour later
                let time = OpeningHoursParser::parse_time(range_part)?;
                let end_hour = if time.hour == 23 { 0 } else { time.hour + 1 };
                ranges.push(TimeRange {
                    start: time,
                    end: Time::new(end_hour, time.minute)?,
                });
            }
        }

        Ok(ranges)
    }

    fn parse_time(input: &str) -> Result<Time, String> {
        let input = input.trim();

        if input.contains(':') {
            let parts: Vec<&str> = input.split(':').collect();
            if parts.len() != 2 {
                return Err("Invalid time format".to_string());
            }

            let hour: u8 = parts[0].parse().map_err(|_| "Invalid hour")?;
            let minute: u8 = parts[1].parse().map_err(|_| "Invalid minute")?;

            Time::new(hour, minute)
        } else {
            // Assume it's just hours
            let hour: u8 = input.parse().map_err(|_| "Invalid hour")?;
            Time::new(hour, 0)
        }
    }
}

// Utility functions for working with parsed opening hours
impl OpeningHours {
    pub fn is_open_at(&self, TimeOfWeek { time, weekday }: &TimeOfWeek) -> bool {
        for rule in &self.rules {
            if rule.closed {
                continue;
            }

            // Check if this rule applies to the given weekday
            let applies_to_weekday = match &rule.weekdays {
                None => true, // No weekday restriction means all days
                Some(WeekdayRange::Single(day)) => day == weekday,
                Some(WeekdayRange::List(days)) => days.contains(weekday),
                Some(WeekdayRange::Range(_start, _end)) => {
                    // This is simplified - proper range checking would need day ordering
                    true // For now, assume it matches
                }
            };

            if applies_to_weekday {
                for time_range in &rule.times {
                    if self.time_in_range(time, &time_range.start, &time_range.end) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn time_in_range(&self, time: &Time, start: &Time, end: &Time) -> bool {
        let time_minutes = time.hour as u16 * 60 + time.minute as u16;
        let start_minutes = start.hour as u16 * 60 + start.minute as u16;
        let end_minutes = end.hour as u16 * 60 + end.minute as u16;

        if start_minutes <= end_minutes {
            time_minutes >= start_minutes && time_minutes <= end_minutes
        } else {
            // Handle overnight ranges
            time_minutes >= start_minutes || time_minutes <= end_minutes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_24_7() {
        let result = OpeningHoursParser::parse("24/7").unwrap();
        assert_eq!(result.rules.len(), 1);
        assert!(!result.rules[0].closed);
    }

    #[test]
    fn test_simple_time_range() {
        let result = OpeningHoursParser::parse("09:00-17:00").unwrap();
        assert_eq!(result.rules.len(), 1);
        assert_eq!(result.rules[0].times.len(), 1);
        assert_eq!(result.rules[0].times[0].start.hour, 9);
        assert_eq!(result.rules[0].times[0].end.hour, 17);
    }

    #[test]
    fn test_multiple_hours() {
        let result = OpeningHoursParser::parse("Mo-Fr 07:00-9:00,16:00-20:00").unwrap();
        assert_eq!(result.rules.len(), 1);
        assert!(matches!(
            result.rules[0].clone().weekdays.unwrap(),
            WeekdayRange::Range(Weekday::Monday, Weekday::Friday)
        ));

        assert_eq!(result.rules[0].times[0].start.hour, 7);
        assert_eq!(result.rules[0].times[0].end.hour, 9);

        assert_eq!(result.rules[0].times[1].start.hour, 16);
        assert_eq!(result.rules[0].times[1].end.hour, 20);
    }

    #[test]
    fn test_weekday_with_time() {
        let result = OpeningHoursParser::parse("Mo-Fr 09:00-17:00").unwrap();
        assert_eq!(result.rules.len(), 1);
        assert!(result.rules[0].weekdays.is_some());
    }

    #[test]
    fn test_multiple_rules() {
        let result = OpeningHoursParser::parse("Mo-Fr 09:00-17:00; Sa 10:00-14:00").unwrap();
        assert_eq!(result.rules.len(), 2);
    }

    #[test]
    fn test_closed() {
        let result = OpeningHoursParser::parse("closed").unwrap();
        assert_eq!(result.rules.len(), 1);
        assert!(result.rules[0].closed);
    }

    #[test]
    fn test_is_open_at() {
        let hours = OpeningHoursParser::parse("Mo-Fr 09:00-17:00").unwrap();

        let monday_noon = Time::new(12, 0).unwrap();
        assert!(hours.is_open_at(&TimeOfWeek {
            weekday: Weekday::Monday,
            time: monday_noon
        }));

        let monday_early = Time::new(8, 0).unwrap();
        assert!(!hours.is_open_at(&TimeOfWeek {
            weekday: Weekday::Monday,
            time: monday_early
        }));
    }
}

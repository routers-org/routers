use chrono::{DateTime, Datelike, Duration, Weekday};
use serde::{Deserialize, Deserializer};
use core::mem;
use core::ops::Add;

use crate::datasource::date::UtcDate;

#[derive(Default, Copy, Clone, Debug, PartialEq, Deserialize)]
pub enum RangeType {
    #[default]
    Inclusive,
    Exclusive,
}

#[derive(Copy, Clone, Debug, Deserialize)]
pub struct Range<T> {
    start: T,
    end: T,
    pub variant: RangeType,
}

impl<T> Range<T>
where
    T: PartialOrd,
{
    pub(crate) fn split_mut(&mut self) -> (&mut T, &mut T) {
        (&mut self.start, &mut self.end)
    }

    pub(crate) fn start(&self) -> &T {
        &self.start
    }

    pub(crate) fn end(&self) -> &T {
        &self.end
    }

    pub fn within<K: Into<T>>(&self, other: K) -> bool {
        let as_current = other.into();

        match self.variant {
            RangeType::Exclusive => self.start < as_current && self.end > as_current,
            RangeType::Inclusive => self.start <= as_current && self.end >= as_current,
        }
    }
}

impl Iterator for Range<UtcDate> {
    type Item = UtcDate;

    fn next(&mut self) -> Option<Self::Item> {
        if self.start() <= self.end() {
            let next = self.start().0.add(Duration::days(1));
            Some(mem::replace(self.split_mut().0, UtcDate(next)))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct Weekdays(Vec<Weekday>);

impl<'de> Deserialize<'de> for Weekdays {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let days = s
            .split('-')
            .map(|s| s.parse::<u32>())
            .collect::<Result<Vec<u32>, _>>()
            .map_err(serde::de::Error::custom)?
            .into_iter()
            .map(|n| match n {
                0 => Ok(Weekday::Mon),
                1 => Ok(Weekday::Tue),
                2 => Ok(Weekday::Wed),
                3 => Ok(Weekday::Thu),
                4 => Ok(Weekday::Fri),
                5 => Ok(Weekday::Sat),
                6 => Ok(Weekday::Sun),
                _ => Err(serde::de::Error::custom("invalid weekday number")),
            })
            .collect::<Result<Vec<Weekday>, _>>()?;

        Ok(Weekdays(days))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DatedRange {
    pub(crate) dates: Range<UtcDate>,
    days: Weekdays,
}

impl DatedRange {
    pub fn within(&self, timestamp: i64) -> bool {
        DateTime::from_timestamp(timestamp, 0).is_some_and(|date| {
            self.dates.within(UtcDate(date)) && self.days.0.contains(&date.weekday())
        })
    }
}

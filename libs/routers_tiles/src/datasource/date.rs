use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use log::error;
use serde::{Deserialize, Deserializer, de};

const STORAGE_FORMAT: &str = "%y%m%d";
const REQUEST_FORMAT: &str = "%Y-%m-%d";

#[derive(Copy, Clone, Debug)]
pub struct UtcDate(pub DateTime<Utc>);

impl PartialOrd for UtcDate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl PartialEq for UtcDate {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

pub fn format_date(date: &DateTime<Utc>) -> String {
    date.format(STORAGE_FORMAT).to_string()
}

impl<'de> Deserialize<'de> for UtcDate {
    fn deserialize<D>(deserializer: D) -> Result<UtcDate, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer) {
            Err(err) => Err(err),
            Ok(date) => match NaiveDate::parse_from_str(&date, REQUEST_FORMAT) {
                Err(err) => {
                    error!("{err}");
                    Err(de::Error::custom(err.to_string()))
                }
                Ok(date) => Ok(UtcDate(Utc.from_utc_datetime(&NaiveDateTime::from(date)))),
            },
        }
    }
}

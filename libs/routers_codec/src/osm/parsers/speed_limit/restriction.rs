use serde::Serialize;

use crate::osm::primitives::{Directionality, TransportMode};
use std::fmt::Display;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
pub struct Restriction {
    /// The transport mode by which the user is travelling.
    /// This may be omitted if not specified, therefore optional.
    ///
    /// See: https://wiki.openstreetmap.org/wiki/Key:access#Transport_mode_restrictions
    pub(crate) transport_mode: TransportMode,

    /// The directionality limit from which the user is travelling.
    /// Can be used to limit roadways which only permit travel in particular directions.
    ///
    /// See: https://wiki.openstreetmap.org/wiki/Forward_%26_backward,_left_%26_right
    pub(crate) directionality: Directionality,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
pub struct RestrictionOptionals {
    /// The transport mode by which the user is travelling.
    /// This may be omitted if not specified, therefore optional.
    ///
    /// See: https://wiki.openstreetmap.org/wiki/Key:access#Transport_mode_restrictions
    pub(crate) transport_mode: Option<TransportMode>,

    /// The directionality limit from which the user is travelling.
    /// Can be used to limit roadways which only permit travel in particular directions.
    ///
    /// See: https://wiki.openstreetmap.org/wiki/Forward_%26_backward,_left_%26_right
    pub(crate) directionality: Option<Directionality>,
}

impl Restriction {
    pub fn parse_require_transport_mode(label: &str) -> Option<Restriction> {
        let builder = Self::parse_builder(label);

        Some(Restriction {
            transport_mode: builder.transport_mode?,
            directionality: builder.directionality.unwrap_or_default(),
        })
    }

    pub fn parse(label: &str) -> RestrictionOptionals {
        Self::parse_builder(label)
    }

    fn parse_builder(label: &str) -> RestrictionOptionals {
        label.split(":").fold(
            RestrictionOptionals {
                transport_mode: None,
                directionality: None,
            },
            |acc, section| RestrictionOptionals {
                transport_mode: acc.transport_mode.or(TransportMode::from_str(section).ok()),
                directionality: acc
                    .directionality
                    .or(Directionality::from_str(section).ok()),
            },
        )
    }
}

impl Display for Restriction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.transport_mode, self.directionality)
    }
}

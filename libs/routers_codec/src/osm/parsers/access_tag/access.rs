use crate::osm::speed_limit::restriction::Restriction;
use crate::osm::{Parser, TagString, Tags};

use strum::{AsRefStr, Display, EnumIter, EnumString};

/// Top-level access restrictions that apply to all transport modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, EnumIter, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum AccessValue {
    /// Public access, legal right of way
    Yes,
    /// Access prohibited by law
    No,
    /// Private property, owner's permission required
    Private,
    /// Tolerated access, permission may be withdrawn
    Permissive,
    /// Explicit designation for this mode (shown by signs/markings)
    Designated,
    /// Legal access exists but officially discouraged
    Discouraged,
    /// Access restricted to customers only
    Customers,
    /// Access restricted to local traffic/destination only
    Destination,
    /// Access restricted to agricultural vehicles
    Agricultural,
    /// Access restricted to forestry vehicles
    Forestry,
    /// Access restricted to delivery vehicles
    Delivery,
    /// Military access only
    Military,
    /// Must use designated parallel way instead
    UseSidepath,
    /// Must dismount and walk (primarily for bicycles)
    Dismount,
    /// Permit required for access
    Permit,
    /// Access status unknown
    Unknown,
    /// Variable access (e.g., tidal roads, variable-access lanes)
    Variable,
}

/// Physical accessibility restrictions (not legal restrictions)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Display, EnumString, EnumIter, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum PhysicalAccess {
    /// Physical accessibility for wheelchairs
    #[strum(serialize = "wheelchair")]
    Wheelchair,
    /// Physical accessibility for baby strollers
    #[strum(serialize = "stroller")]
    Stroller,
}

/// Main parser structure for OSM access tags
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccessTag {
    pub restriction: Restriction,
    pub access: AccessValue,
}

impl AccessTag {
    /// Parse an OSM access tag from key-value strings
    ///
    /// # Examples
    /// ```
    /// use std::str::FromStr;
    /// use routers_routers_codec::osm::access_tag::AccessTag;
    ///
    /// // Parse "bicycle=no"
    /// let tag = AccessTag::from_key_value("bicycle", "no")?;
    ///
    /// // Parse "motor_vehicle=destination"
    /// let tag = AccessTag::from_key_value("motor_vehicle", "destination")?;
    /// ```
    pub fn from_key_value(key: &str, value: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let access = AccessValue::try_from(value)?;
        let restriction = Restriction::parse_require_transport_mode(key)
            .ok_or("Key not found in require transport mode")?;

        Ok(AccessTag {
            restriction,
            access,
        })
    }

    fn from_tag((key, value): (&TagString, &TagString)) -> Option<Self> {
        Self::from_key_value(key, value).ok()
    }

    #[cfg(test)]
    fn to_key_value(&self) -> (String, String) {
        (self.restriction.to_string(), self.access.to_string())
    }
}

impl Parser for Vec<AccessTag> {
    fn parse(tags: &Tags) -> Option<Self> {
        let as_vec = tags
            .iter()
            .filter_map(AccessTag::from_tag)
            .collect::<Vec<_>>();

        if as_vec.is_empty() {
            None
        } else {
            Some(as_vec)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osm::primitives::TransportMode;
    use std::str::FromStr;

    #[test]
    fn test_access_value_parsing() {
        assert_eq!(AccessValue::from_str("yes").unwrap(), AccessValue::Yes);
        assert_eq!(AccessValue::from_str("no").unwrap(), AccessValue::No);
        assert_eq!(
            AccessValue::from_str("private").unwrap(),
            AccessValue::Private
        );
        assert_eq!(
            AccessValue::from_str("destination").unwrap(),
            AccessValue::Destination
        );
    }

    #[test]
    fn test_transport_mode_parsing() {
        let tag = AccessTag::from_key_value("bicycle", "no").unwrap();
        assert_eq!(tag.access, AccessValue::No);

        let tag = AccessTag::from_key_value("motor_vehicle", "destination").unwrap();
        assert_eq!(tag.access, AccessValue::Destination);
    }

    #[test]
    fn test_pure_mode_parsing() {
        let tag = AccessTag::from_key_value("access", "no").unwrap();
        assert_eq!(tag.access, AccessValue::No);
        assert_eq!(tag.restriction.transport_mode, TransportMode::All);

        let tag = AccessTag::from_key_value("access", "yes").unwrap();
        assert_eq!(tag.access, AccessValue::Yes);
        assert_eq!(tag.restriction.transport_mode, TransportMode::All);
    }

    #[test]
    fn test_round_trip_conversion() {
        let original_tag = AccessTag::from_key_value("foot", "yes").unwrap();
        let (key, value) = original_tag.to_key_value();
        let parsed_tag = AccessTag::from_key_value(&key, &value).unwrap();
        assert_eq!(original_tag, parsed_tag);
    }
}

use serde::Serialize;
use strum::{AsRefStr, Display, EnumIter, EnumString};

/// Represents the directionality modes used in OpenStreetMap (OSM) tagging.
///
/// This enum covers the directional and positional indicators used in OSM
/// to specify direction of travel or side of a way relative to how the way
/// is drawn in the OSM database.
#[derive(
    Serialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    Display,
    EnumString,
    EnumIter,
    AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum Directionality {
    /// Applies to both directions of travel along a way.
    /// Used for features like center turn lanes that serve traffic in both directions.
    /// Example: `turn:lanes:both_ways=left` for a center left-turn lane.
    #[strum(serialize = "both_ways")]
    #[default]
    BothWays = 0,

    /// Direction in which the OSM way is drawn (from first node to last node).
    /// Used in tags like `lanes:forward=2` or `oneway=yes` (implicit forward).
    #[strum(serialize = "forward")]
    Forward = 1,

    /// Direction opposite to how the OSM way is drawn (from last node to first node).
    /// Used in tags like `lanes:backward=1` or `oneway=-1` (backward direction).
    #[strum(serialize = "backward")]
    Backward = 2,

    /// Explicitly indicates a feature applies to both sides of a way.
    /// Used to disambiguate when the base tag might be ambiguous about sidedness.
    /// Example: `cycleway:both=lane` to indicate bike lanes on both sides.
    #[strum(serialize = "both")]
    Both = 3,

    /// Left-hand side of the way when looking in the forward direction.
    /// Used in tags like `cycleway:left=lane` or `parking:lane:left=parallel`.
    #[strum(serialize = "left")]
    Left = 4,

    /// Right-hand side of the way when looking in the forward direction.
    /// Used in tags like `cycleway:right=track` or `sidewalk:right=yes`.
    #[strum(serialize = "right")]
    Right = 5,
}

impl Directionality {
    /// Returns true if this directionality represents a direction along the way
    /// (forward/backward/both_ways) rather than a side of the way (left/right/both).
    pub fn is_directional(&self) -> bool {
        matches!(self, Self::Forward | Self::Backward | Self::BothWays)
    }

    /// Returns true if this directionality represents a side of the way
    /// (left/right/both) rather than a direction along the way.
    pub fn is_positional(&self) -> bool {
        matches!(self, Self::Left | Self::Right | Self::Both)
    }

    /// Returns the opposite direction for directional variants.
    /// Returns None for positional variants or both_ways.
    pub fn opposite(&self) -> Option<Self> {
        match self {
            Self::Forward => Some(Self::Backward),
            Self::Backward => Some(Self::Forward),
            Self::Left => Some(Self::Right),
            Self::Right => Some(Self::Left),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_string_conversion() {
        // Test string to enum conversion
        assert_eq!(
            Directionality::from_str("forward").unwrap(),
            Directionality::Forward
        );
        assert_eq!(
            Directionality::from_str("backward").unwrap(),
            Directionality::Backward
        );
        assert_eq!(
            Directionality::from_str("left").unwrap(),
            Directionality::Left
        );
        assert_eq!(
            Directionality::from_str("right").unwrap(),
            Directionality::Right
        );
        assert_eq!(
            Directionality::from_str("both_ways").unwrap(),
            Directionality::BothWays
        );
        assert_eq!(
            Directionality::from_str("both").unwrap(),
            Directionality::Both
        );

        // Test enum to string conversion
        assert_eq!(Directionality::Forward.to_string(), "forward");
        assert_eq!(Directionality::Backward.to_string(), "backward");
        assert_eq!(Directionality::Left.to_string(), "left");
        assert_eq!(Directionality::Right.to_string(), "right");
        assert_eq!(Directionality::BothWays.to_string(), "both_ways");
        assert_eq!(Directionality::Both.to_string(), "both");
    }

    #[test]
    fn test_directional_vs_positional() {
        assert!(Directionality::Forward.is_directional());
        assert!(Directionality::Backward.is_directional());
        assert!(Directionality::BothWays.is_directional());

        assert!(Directionality::Left.is_positional());
        assert!(Directionality::Right.is_positional());
        assert!(Directionality::Both.is_positional());

        // Ensure they're mutually exclusive
        for variant in [
            Directionality::Forward,
            Directionality::Backward,
            Directionality::Left,
            Directionality::Right,
            Directionality::BothWays,
            Directionality::Both,
        ] {
            assert_ne!(variant.is_directional(), variant.is_positional());
        }
    }

    #[test]
    fn test_opposite_direction() {
        assert_eq!(
            Directionality::Forward.opposite(),
            Some(Directionality::Backward)
        );
        assert_eq!(
            Directionality::Backward.opposite(),
            Some(Directionality::Forward)
        );
        assert_eq!(Directionality::Left.opposite(), Some(Directionality::Right));
        assert_eq!(Directionality::Right.opposite(), Some(Directionality::Left));
        assert_eq!(Directionality::BothWays.opposite(), None);
        assert_eq!(Directionality::Both.opposite(), None);
    }

    #[test]
    fn test_invalid_string() {
        assert!(Directionality::from_str("invalid").is_err());
        assert!(Directionality::from_str("forwards").is_err()); // Note the 's'
        assert!(Directionality::from_str("FORWARD").is_err()); // Case sensitive
    }
}

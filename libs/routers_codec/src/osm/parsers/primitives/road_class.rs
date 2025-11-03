use serde::Serialize;
use strum::{AsRefStr, Display, EnumString};

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum RoadClass {
    /// A restricted access major divided highway, normally with 2 or more
    /// running lanes plus emergency hard shoulder. Equivalent to the Freeway, Autobahn, etc..
    Motorway,

    /// The link roads (sliproads/ramps) leading to/from a motorway from/to a
    /// motorway or lower class highway. Normally with the same motorway restrictions.
    MotorwayLink,

    /// The most important roads in a country's system that aren't motorways.
    /// (Need not necessarily be a divided highway.)
    Trunk,

    /// The link roads (sliproads/ramps) leading to/from a trunk road
    /// from/to a trunk road or lower class highway.
    TrunkLink,

    /// The next most important roads in a country's system.
    /// (Often link larger towns.)
    Primary,

    /// The link roads (sliproads/ramps) leading to/from a primary road
    /// from/to a primary road or lower class highway.
    PrimaryLink,

    /// The next most important roads in a country's system.
    /// (Often link towns.)
    Secondary,

    /// The link roads (sliproads/ramps) leading to/from a secondary road
    /// from/to a secondary road or lower class highway.
    SecondaryLink,

    /// The next most important roads in a country's system.
    /// (Often link smaller towns and villages)
    Tertiary,

    /// The link roads (sliproads/ramps) leading to/from a tertiary road
    /// from/to a tertiary road or lower class highway.
    TertiaryLink,

    /// The least important through roads in a country's system.
    /// i.e. minor roads of a lower classification than tertiary, but which serve a
    ///     purpose other than access to properties. (Often link villages and hamlets.)
    ///
    /// The word 'unclassified' is a historical artefact of the UK road system and does
    /// not mean that the classification is unknown; you can use highway=road for that.
    Unclassified,

    /// Roads which serve as access to housing, without function
    /// of connecting settlements. Often lined with housing.
    Residential,

    // Special Road Types
    /// For living streets, which are residential streets where pedestrians have legal
    /// priority over cars, speeds are kept very low.
    LivingStreet,

    /// For access roads to, or within an industrial estate, camp site, business park, car park,
    /// alleys, etc. Can be used in conjunction with service=* to indicate the type of usage
    /// and with access=* to indicate who can use it and in what circumstances.
    Service,

    /// For roads used mainly/exclusively for pedestrians in shopping and some residential areas
    /// which may allow access by motorised vehicles only for very limited periods of the day.
    ///
    /// To create a 'square' or 'plaza' create a closed way and tag as pedestrian and also with
    /// area=yes.
    Pedestrian,

    /// Roads for mostly agricultural or forestry uses.
    /// To describe the quality of a track, see tracktype=*.
    ///
    /// Note: Although tracks are often rough with unpaved surfaces, this tag is not describing
    /// the quality of a road but its use.
    ///
    /// Consequently, if you want to tag a general use road, use one of the general highway
    /// values instead of track.
    Track,

    /// A course or track for (motor) racing
    Raceway,

    /// A busway where the vehicle guided by the way (though not a railway)
    /// and is not suitable for other traffic.
    ///
    /// Note: this is not a normal bus lane, use access=no, psv=yes instead!
    BusGuideway,

    /// A dedicated roadway for bus rapid transit systems
    Busway,

    /// For runaway truck ramps, runaway truck lanes, emergency escape ramps,
    /// or truck arrester beds. It enables vehicles with braking failure to safely stop.
    Escape,

    /// A road/way/street/motorway/etc. of unknown type.
    ///
    /// It can stand for anything ranging from a footpath to a motorway.
    /// This tag should only be used temporarily until the road/way/etc.
    /// has been properly surveyed.
    ///
    /// If you do know the road type, do not use this value, instead use one
    /// of the more specific highway=* values.
    Road,
}

impl RoadClass {
    #[inline]
    pub const fn weighting(&self) -> u32 {
        match self {
            RoadClass::Motorway => 1,
            RoadClass::MotorwayLink => 2,
            RoadClass::Trunk => 3,
            RoadClass::TrunkLink => 4,
            RoadClass::Primary => 5,
            RoadClass::PrimaryLink => 6,
            RoadClass::Secondary => 7,
            RoadClass::SecondaryLink => 8,
            RoadClass::Tertiary => 9,
            RoadClass::TertiaryLink => 10,

            // Residential / Assoc.
            RoadClass::Residential => 10,
            RoadClass::Busway => 10,
            RoadClass::BusGuideway => 10,
            RoadClass::Unclassified => 10,

            // Misc / Service. (Shouldn't be impossible to traverse, just difficult.)
            RoadClass::LivingStreet => 50,
            RoadClass::Service => 50,
            RoadClass::Road => 50,
            RoadClass::Raceway => 100,
            RoadClass::Escape => 100,
            RoadClass::Track => 100,
            RoadClass::Pedestrian => 100,
        }
    }
}

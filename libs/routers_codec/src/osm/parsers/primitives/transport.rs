use crate::osm::primitives::transport::bitflag::TransportModeSet;
use serde::Serialize;
use strum::{AsRefStr, Display, EnumString};

/// Flattened transport mode enumeration for easy string parsing
#[derive(
    Serialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Display, EnumString, AsRefStr,
)]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum TransportMode {
    // General access
    #[strum(serialize = "access")]
    #[default]
    All,

    // === LAND TRANSPORT ===

    // Non-vehicle transport
    #[strum(serialize = "foot")]
    Foot,
    #[strum(serialize = "dog")]
    Dog,
    #[strum(serialize = "ski")]
    Ski,
    #[strum(serialize = "ski:nordic")]
    SkiNordic,
    #[strum(serialize = "ski:alpine")]
    SkiAlpine,
    #[strum(serialize = "ski:telemark")]
    SkiTelemark,
    #[strum(serialize = "inline_skates")]
    InlineSkates,
    #[strum(serialize = "horse")]
    Horse,
    #[strum(serialize = "portage")]
    Portage,

    // Non-motorized single-tracked vehicles
    #[strum(serialize = "bicycle")]
    Bicycle,
    #[strum(serialize = "electric_bicycle")]
    ElectricBicycle,
    #[strum(serialize = "mtb")]
    Mtb,
    #[strum(serialize = "cargo_bike")]
    CargoBike,
    #[strum(serialize = "kick_scooter")]
    KickScooter,

    // Non-motorized double-tracked vehicles
    #[strum(serialize = "carriage")]
    Carriage,
    #[strum(serialize = "cycle_rickshaw")]
    CycleRickshaw,
    #[strum(serialize = "hand_cart")]
    HandCart,
    #[strum(serialize = "trailer")]
    Trailer,
    #[strum(serialize = "caravan")]
    Caravan,

    // General vehicle categories
    #[strum(serialize = "vehicle")]
    Vehicle,
    #[strum(serialize = "motor_vehicle")]
    MotorVehicle,

    // Motorized single-tracked vehicles
    #[strum(serialize = "motorcycle")]
    Motorcycle,
    #[strum(serialize = "moped")]
    Moped,
    #[strum(serialize = "speed_pedelec")]
    SpeedPedelec,
    #[strum(serialize = "mofa")]
    Mofa,
    #[strum(serialize = "small_electric_vehicle")]
    SmallElectricVehicle,

    // Motorized double-tracked vehicles
    #[strum(serialize = "motorcar")]
    Motorcar,
    #[strum(serialize = "motorhome")]
    Motorhome,
    #[strum(serialize = "tourist_bus")]
    TouristBus,
    #[strum(serialize = "coach")]
    Coach,
    #[strum(serialize = "goods")]
    Goods,
    #[strum(serialize = "hgv")]
    /// HGV stands for Heavy Goods Vehicle
    Hgv,
    #[strum(serialize = "hgv_articulated")]
    HgvArticulated,
    #[strum(serialize = "bdouble")]
    Bdouble,
    #[strum(serialize = "agricultural")]
    Agricultural,
    #[strum(serialize = "auto_rickshaw")]
    AutoRickshaw,
    #[strum(serialize = "nev")]
    Nev,
    #[strum(serialize = "golf_cart")]
    GolfCart,
    #[strum(serialize = "microcar")]
    Microcar,
    #[strum(serialize = "atv")]
    Atv,
    #[strum(serialize = "ohv")]
    Ohv,
    #[strum(serialize = "snowmobile")]
    Snowmobile,

    // Vehicles by use/purpose
    #[strum(serialize = "psv")]
    Psv,
    #[strum(serialize = "bus")]
    Bus,
    #[strum(serialize = "taxi")]
    Taxi,
    #[strum(serialize = "minibus")]
    Minibus,
    #[strum(serialize = "share_taxi")]
    ShareTaxi,
    #[strum(serialize = "hov")]
    Hov,
    #[strum(serialize = "carpool")]
    Carpool,
    #[strum(serialize = "car_sharing")]
    CarSharing,
    #[strum(serialize = "emergency")]
    Emergency,
    #[strum(serialize = "hazmat")]
    Hazmat,
    #[strum(serialize = "hazmat:water")]
    HazmatWater,
    #[strum(serialize = "school_bus")]
    SchoolBus,
    #[strum(serialize = "disabled")]
    Disabled,

    // === WATER TRANSPORT ===
    #[strum(serialize = "water_access")]
    WaterAccess,
    #[strum(serialize = "swimming")]
    Swimming,
    #[strum(serialize = "ice_skates")]
    IceSkates,
    #[strum(serialize = "boat")]
    Boat,
    #[strum(serialize = "motorboat")]
    Motorboat,
    #[strum(serialize = "sailboat")]
    Sailboat,
    #[strum(serialize = "canoe")]
    Canoe,
    #[strum(serialize = "fishing_vessel")]
    FishingVessel,
    #[strum(serialize = "ship")]
    Ship,
    #[strum(serialize = "passenger")]
    Passenger,
    #[strum(serialize = "cargo")]
    Cargo,
    #[strum(serialize = "bulk")]
    Bulk,
    #[strum(serialize = "tanker")]
    Tanker,
    #[strum(serialize = "tanker:gas")]
    TankerGas,
    #[strum(serialize = "tanker:oil")]
    TankerOil,
    #[strum(serialize = "tanker:chemical")]
    TankerChemical,
    #[strum(serialize = "tanker:singlehull")]
    TankerSinglehull,
    #[strum(serialize = "container")]
    Container,
    #[strum(serialize = "imdg")]
    Imdg,
    #[strum(serialize = "isps")]
    Isps,

    // === RAIL TRANSPORT ===
    #[strum(serialize = "rail_access")]
    RailAccess,
    // Add more rail-specific modes as needed
    #[strum(serialize = "train")]
    Train,
    #[strum(serialize = "tram")]
    Tram,
    #[strum(serialize = "metro")]
    Metro,
}

mod bitflag {
    use super::TransportMode;
    use bitflags::bitflags;

    bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct TransportModeSet: u128 {
            const ALL = 1 << 0;

            // === LAND TRANSPORT ===
            // Non-vehicle transport
            const FOOT = 1 << 1;
            const DOG = 1 << 2;
            const SKI = 1 << 3;
            const SKI_NORDIC = 1 << 4;
            const SKI_ALPINE = 1 << 5;
            const SKI_TELEMARK = 1 << 6;
            const INLINE_SKATES = 1 << 7;
            const HORSE = 1 << 8;
            const PORTAGE = 1 << 9;

            // Non-motorized single-tracked vehicles
            const BICYCLE = 1 << 10;
            const ELECTRIC_BICYCLE = 1 << 11;
            const MTB = 1 << 12;
            const CARGO_BIKE = 1 << 13;
            const KICK_SCOOTER = 1 << 14;

            // Non-motorized double-tracked vehicles
            const CARRIAGE = 1 << 15;
            const CYCLE_RICKSHAW = 1 << 16;
            const HAND_CART = 1 << 17;
            const TRAILER = 1 << 18;
            const CARAVAN = 1 << 19;

            // General vehicle categories
            const VEHICLE = 1 << 20;
            const MOTOR_VEHICLE = 1 << 21;

            // Motorized single-tracked vehicles
            const MOTORCYCLE = 1 << 22;
            const MOPED = 1 << 23;
            const SPEED_PEDELEC = 1 << 24;
            const MOFA = 1 << 25;
            const SMALL_ELECTRIC_VEHICLE = 1 << 26;

            // Motorized double-tracked vehicles
            const MOTORCAR = 1 << 27;
            const MOTORHOME = 1 << 28;
            const TOURIST_BUS = 1 << 29;
            const COACH = 1 << 30;
            const GOODS = 1 << 31;
            const HGV = 1 << 32;
            const HGV_ARTICULATED = 1 << 33;
            const BDOUBLE = 1 << 34;
            const AGRICULTURAL = 1 << 35;
            const AUTO_RICKSHAW = 1 << 36;
            const NEV = 1 << 37;
            const GOLF_CART = 1 << 38;
            const MICROCAR = 1 << 39;
            const ATV = 1 << 40;
            const OHV = 1 << 41;
            const SNOWMOBILE = 1 << 42;

            // Vehicles by use/purpose
            const PSV = 1 << 43;
            const BUS = 1 << 44;
            const TAXI = 1 << 45;
            const MINIBUS = 1 << 46;
            const SHARE_TAXI = 1 << 47;
            const HOV = 1 << 48;
            const CARPOOL = 1 << 49;
            const CAR_SHARING = 1 << 50;
            const EMERGENCY = 1 << 51;
            const HAZMAT = 1 << 52;
            const HAZMAT_WATER = 1 << 53;
            const SCHOOL_BUS = 1 << 54;
            const DISABLED = 1 << 55;

            // === WATER TRANSPORT ===
            const WATER_ACCESS = 1 << 56;
            const SWIMMING = 1 << 57;
            const ICE_SKATES = 1 << 58;
            const BOAT = 1 << 59;
            const MOTORBOAT = 1 << 60;
            const SAILBOAT = 1 << 61;
            const CANOE = 1 << 62;
            const FISHING_VESSEL = 1 << 63;
            const SHIP = 1 << 64;
            const PASSENGER = 1 << 65;
            const CARGO = 1 << 66;
            const BULK = 1 << 67;
            const TANKER = 1 << 68;
            const TANKER_GAS = 1 << 69;
            const TANKER_OIL = 1 << 70;
            const TANKER_CHEMICAL = 1 << 71;
            const TANKER_SINGLEHULL = 1 << 72;
            const CONTAINER = 1 << 73;
            const IMDG = 1 << 74;
            const ISPS = 1 << 75;

            // === RAIL TRANSPORT ===
            const RAIL_ACCESS = 1 << 76;
            const TRAIN = 1 << 77;
            const TRAM = 1 << 78;
            const METRO = 1 << 79;
        }
    }

    impl TransportMode {
        pub const fn to_flag(self) -> TransportModeSet {
            match self {
                TransportMode::All => TransportModeSet::ALL,

                // === LAND TRANSPORT ===
                // Non-vehicle transport
                TransportMode::Foot => TransportModeSet::FOOT,
                TransportMode::Dog => TransportModeSet::DOG,
                TransportMode::Ski => TransportModeSet::SKI,
                TransportMode::SkiNordic => TransportModeSet::SKI_NORDIC,
                TransportMode::SkiAlpine => TransportModeSet::SKI_ALPINE,
                TransportMode::SkiTelemark => TransportModeSet::SKI_TELEMARK,
                TransportMode::InlineSkates => TransportModeSet::INLINE_SKATES,
                TransportMode::Horse => TransportModeSet::HORSE,
                TransportMode::Portage => TransportModeSet::PORTAGE,

                // Non-motorized single-tracked vehicles
                TransportMode::Bicycle => TransportModeSet::BICYCLE,
                TransportMode::ElectricBicycle => TransportModeSet::ELECTRIC_BICYCLE,
                TransportMode::Mtb => TransportModeSet::MTB,
                TransportMode::CargoBike => TransportModeSet::CARGO_BIKE,
                TransportMode::KickScooter => TransportModeSet::KICK_SCOOTER,

                // Non-motorized double-tracked vehicles
                TransportMode::Carriage => TransportModeSet::CARRIAGE,
                TransportMode::CycleRickshaw => TransportModeSet::CYCLE_RICKSHAW,
                TransportMode::HandCart => TransportModeSet::HAND_CART,
                TransportMode::Trailer => TransportModeSet::TRAILER,
                TransportMode::Caravan => TransportModeSet::CARAVAN,

                // General vehicle categories
                TransportMode::Vehicle => TransportModeSet::VEHICLE,
                TransportMode::MotorVehicle => TransportModeSet::MOTOR_VEHICLE,

                // Motorized single-tracked vehicles
                TransportMode::Motorcycle => TransportModeSet::MOTORCYCLE,
                TransportMode::Moped => TransportModeSet::MOPED,
                TransportMode::SpeedPedelec => TransportModeSet::SPEED_PEDELEC,
                TransportMode::Mofa => TransportModeSet::MOFA,
                TransportMode::SmallElectricVehicle => TransportModeSet::SMALL_ELECTRIC_VEHICLE,

                // Motorized double-tracked vehicles
                TransportMode::Motorcar => TransportModeSet::MOTORCAR,
                TransportMode::Motorhome => TransportModeSet::MOTORHOME,
                TransportMode::TouristBus => TransportModeSet::TOURIST_BUS,
                TransportMode::Coach => TransportModeSet::COACH,
                TransportMode::Goods => TransportModeSet::GOODS,
                TransportMode::Hgv => TransportModeSet::HGV,
                TransportMode::HgvArticulated => TransportModeSet::HGV_ARTICULATED,
                TransportMode::Bdouble => TransportModeSet::BDOUBLE,
                TransportMode::Agricultural => TransportModeSet::AGRICULTURAL,
                TransportMode::AutoRickshaw => TransportModeSet::AUTO_RICKSHAW,
                TransportMode::Nev => TransportModeSet::NEV,
                TransportMode::GolfCart => TransportModeSet::GOLF_CART,
                TransportMode::Microcar => TransportModeSet::MICROCAR,
                TransportMode::Atv => TransportModeSet::ATV,
                TransportMode::Ohv => TransportModeSet::OHV,
                TransportMode::Snowmobile => TransportModeSet::SNOWMOBILE,

                // Vehicles by use/purpose
                TransportMode::Psv => TransportModeSet::PSV,
                TransportMode::Bus => TransportModeSet::BUS,
                TransportMode::Taxi => TransportModeSet::TAXI,
                TransportMode::Minibus => TransportModeSet::MINIBUS,
                TransportMode::ShareTaxi => TransportModeSet::SHARE_TAXI,
                TransportMode::Hov => TransportModeSet::HOV,
                TransportMode::Carpool => TransportModeSet::CARPOOL,
                TransportMode::CarSharing => TransportModeSet::CAR_SHARING,
                TransportMode::Emergency => TransportModeSet::EMERGENCY,
                TransportMode::Hazmat => TransportModeSet::HAZMAT,
                TransportMode::HazmatWater => TransportModeSet::HAZMAT_WATER,
                TransportMode::SchoolBus => TransportModeSet::SCHOOL_BUS,
                TransportMode::Disabled => TransportModeSet::DISABLED,

                // === WATER TRANSPORT ===
                TransportMode::WaterAccess => TransportModeSet::WATER_ACCESS,
                TransportMode::Swimming => TransportModeSet::SWIMMING,
                TransportMode::IceSkates => TransportModeSet::ICE_SKATES,
                TransportMode::Boat => TransportModeSet::BOAT,
                TransportMode::Motorboat => TransportModeSet::MOTORBOAT,
                TransportMode::Sailboat => TransportModeSet::SAILBOAT,
                TransportMode::Canoe => TransportModeSet::CANOE,
                TransportMode::FishingVessel => TransportModeSet::FISHING_VESSEL,
                TransportMode::Ship => TransportModeSet::SHIP,
                TransportMode::Passenger => TransportModeSet::PASSENGER,
                TransportMode::Cargo => TransportModeSet::CARGO,
                TransportMode::Bulk => TransportModeSet::BULK,
                TransportMode::Tanker => TransportModeSet::TANKER,
                TransportMode::TankerGas => TransportModeSet::TANKER_GAS,
                TransportMode::TankerOil => TransportModeSet::TANKER_OIL,
                TransportMode::TankerChemical => TransportModeSet::TANKER_CHEMICAL,
                TransportMode::TankerSinglehull => TransportModeSet::TANKER_SINGLEHULL,
                TransportMode::Container => TransportModeSet::CONTAINER,
                TransportMode::Imdg => TransportModeSet::IMDG,
                TransportMode::Isps => TransportModeSet::ISPS,

                // === RAIL TRANSPORT ===
                TransportMode::RailAccess => TransportModeSet::RAIL_ACCESS,
                TransportMode::Train => TransportModeSet::TRAIN,
                TransportMode::Tram => TransportModeSet::TRAM,
                TransportMode::Metro => TransportModeSet::METRO,
            }
        }
    }
}

impl TransportMode {
    /// Get the specificity level (depth in hierarchy) - higher means more specific
    #[inline]
    pub const fn specificity_level(&self) -> usize {
        self.hierarchy_chain().len()
    }

    /// Returns the hierarchy chain from most general to most specific
    #[inline]
    const fn hierarchy_chain(&self) -> &'static [TransportMode] {
        use TransportMode::*;

        match self {
            // === LAND TRANSPORT ===

            // General access
            All => &[All],

            // Non-vehicle transport (directly under land access)
            Foot => &[All, Foot],
            Dog => &[All, Dog],
            Ski => &[All, Ski],
            SkiNordic => &[All, Ski, SkiNordic],
            SkiAlpine => &[All, Ski, SkiAlpine],
            SkiTelemark => &[All, Ski, SkiTelemark],
            InlineSkates => &[All, InlineSkates],
            Horse => &[All, Horse],
            Portage => &[All, Portage],

            // Non-motorized vehicles
            Vehicle => &[All, Vehicle],

            // Non-motorized single-tracked vehicles
            Bicycle => &[All, Vehicle, Bicycle],
            ElectricBicycle => &[All, Vehicle, Bicycle, ElectricBicycle],
            Mtb => &[All, Vehicle, Bicycle, Mtb],
            CargoBike => &[All, Vehicle, Bicycle, CargoBike],
            KickScooter => &[All, Vehicle, KickScooter],

            // Non-motorized double-tracked vehicles
            Carriage => &[All, Vehicle, Carriage],
            CycleRickshaw => &[All, Vehicle, CycleRickshaw],
            HandCart => &[All, Vehicle, HandCart],
            Trailer => &[All, Vehicle, Trailer],
            Caravan => &[All, Vehicle, Caravan],

            // Motor vehicles
            MotorVehicle => &[All, Vehicle, MotorVehicle],

            // Motorized single-tracked vehicles
            Motorcycle => &[All, Vehicle, MotorVehicle, Motorcycle],
            Moped => &[All, Vehicle, MotorVehicle, Moped],
            SpeedPedelec => &[All, Vehicle, MotorVehicle, SpeedPedelec],
            Mofa => &[All, Vehicle, MotorVehicle, Mofa],
            SmallElectricVehicle => &[All, Vehicle, MotorVehicle, SmallElectricVehicle],

            // Motorized double-tracked vehicles
            Motorcar => &[All, Vehicle, MotorVehicle, Motorcar],
            Motorhome => &[All, Vehicle, MotorVehicle, Motorhome],
            TouristBus => &[All, Vehicle, MotorVehicle, TouristBus],
            Coach => &[All, Vehicle, MotorVehicle, Coach],
            Goods => &[All, Vehicle, MotorVehicle, Goods],
            Hgv => &[All, Vehicle, MotorVehicle, Goods, Hgv],
            HgvArticulated => &[All, Vehicle, MotorVehicle, Goods, Hgv, HgvArticulated],
            Bdouble => &[All, Vehicle, MotorVehicle, Goods, Hgv, Bdouble],
            Agricultural => &[All, Vehicle, MotorVehicle, Agricultural],
            AutoRickshaw => &[All, Vehicle, MotorVehicle, AutoRickshaw],
            Nev => &[All, Vehicle, MotorVehicle, Nev],
            GolfCart => &[All, Vehicle, MotorVehicle, GolfCart],
            Microcar => &[All, Vehicle, MotorVehicle, Microcar],
            Atv => &[All, Vehicle, MotorVehicle, Atv],
            Ohv => &[All, Vehicle, MotorVehicle, Ohv],
            Snowmobile => &[All, Vehicle, MotorVehicle, Snowmobile],

            // Vehicles by use/purpose (these often inherit from motor_vehicle or specific types)
            Psv => &[All, Vehicle, MotorVehicle, Psv],
            Bus => &[All, Vehicle, MotorVehicle, Psv, Bus],
            Taxi => &[All, Vehicle, MotorVehicle, Taxi],
            Minibus => &[All, Vehicle, MotorVehicle, Psv, Bus, Minibus],
            ShareTaxi => &[All, Vehicle, MotorVehicle, Taxi, ShareTaxi],
            Hov => &[All, Vehicle, MotorVehicle, Hov],
            Carpool => &[All, Vehicle, MotorVehicle, Hov, Carpool],
            CarSharing => &[All, Vehicle, MotorVehicle, CarSharing],
            Emergency => &[All, Vehicle, MotorVehicle, Emergency],
            Hazmat => &[All, Vehicle, MotorVehicle, Hazmat],
            HazmatWater => &[All, Vehicle, MotorVehicle, Hazmat, HazmatWater],
            SchoolBus => &[All, Vehicle, MotorVehicle, Psv, Bus, SchoolBus],
            Disabled => &[All, Vehicle, MotorVehicle, Disabled],

            // === WATER TRANSPORT ===
            WaterAccess => &[WaterAccess],
            Swimming => &[WaterAccess, Swimming],
            IceSkates => &[WaterAccess, IceSkates],

            Boat => &[WaterAccess, Boat],
            Motorboat => &[WaterAccess, Boat, Motorboat],
            Sailboat => &[WaterAccess, Boat, Sailboat],
            Canoe => &[WaterAccess, Boat, Canoe],
            FishingVessel => &[WaterAccess, Boat, FishingVessel],

            Ship => &[WaterAccess, Boat, Ship],
            Passenger => &[WaterAccess, Boat, Ship, Passenger],
            Cargo => &[WaterAccess, Boat, Ship, Cargo],
            Bulk => &[WaterAccess, Boat, Ship, Cargo, Bulk],
            Tanker => &[WaterAccess, Boat, Ship, Cargo, Tanker],
            TankerGas => &[WaterAccess, Boat, Ship, Cargo, Tanker, TankerGas],
            TankerOil => &[WaterAccess, Boat, Ship, Cargo, Tanker, TankerOil],
            TankerChemical => &[WaterAccess, Boat, Ship, Cargo, Tanker, TankerChemical],
            TankerSinglehull => &[WaterAccess, Boat, Ship, Cargo, Tanker, TankerSinglehull],
            Container => &[WaterAccess, Boat, Ship, Cargo, Container],
            Imdg => &[WaterAccess, Boat, Ship, Imdg],
            Isps => &[WaterAccess, Boat, Ship, Isps],

            // === RAIL TRANSPORT ===
            RailAccess => &[RailAccess],
            Train => &[RailAccess, Train],
            Tram => &[RailAccess, Tram],
            Metro => &[RailAccess, Metro],
        }
    }

    #[inline]
    pub const fn hierarchy_chain_flags(self) -> TransportModeSet {
        use TransportMode::*;

        match self {
            // === LAND TRANSPORT ===

            // General access
            All => TransportModeSet::ALL,

            // Non-vehicle transport (directly under land access)
            Foot => TransportModeSet::ALL.union(TransportModeSet::FOOT),
            Dog => TransportModeSet::ALL.union(TransportModeSet::DOG),
            Ski => TransportModeSet::ALL.union(TransportModeSet::SKI),
            SkiNordic => TransportModeSet::ALL
                .union(TransportModeSet::SKI)
                .union(TransportModeSet::SKI_NORDIC),
            SkiAlpine => TransportModeSet::ALL
                .union(TransportModeSet::SKI)
                .union(TransportModeSet::SKI_ALPINE),
            SkiTelemark => TransportModeSet::ALL
                .union(TransportModeSet::SKI)
                .union(TransportModeSet::SKI_TELEMARK),
            InlineSkates => TransportModeSet::ALL.union(TransportModeSet::INLINE_SKATES),
            Horse => TransportModeSet::ALL.union(TransportModeSet::HORSE),
            Portage => TransportModeSet::ALL.union(TransportModeSet::PORTAGE),

            // Non-motorized vehicles
            Vehicle => TransportModeSet::ALL.union(TransportModeSet::VEHICLE),

            // Non-motorized single-tracked vehicles
            Bicycle => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::BICYCLE),
            ElectricBicycle => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::BICYCLE)
                .union(TransportModeSet::ELECTRIC_BICYCLE),
            Mtb => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::BICYCLE)
                .union(TransportModeSet::MTB),
            CargoBike => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::BICYCLE)
                .union(TransportModeSet::CARGO_BIKE),
            KickScooter => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::KICK_SCOOTER),

            // Non-motorized double-tracked vehicles
            Carriage => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::CARRIAGE),
            CycleRickshaw => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::CYCLE_RICKSHAW),
            HandCart => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::HAND_CART),
            Trailer => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::TRAILER),
            Caravan => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::CARAVAN),

            // Motor vehicles
            MotorVehicle => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE),

            // Motorized single-tracked vehicles
            Motorcycle => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::MOTORCYCLE),
            Moped => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::MOPED),
            SpeedPedelec => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::SPEED_PEDELEC),
            Mofa => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::MOFA),
            SmallElectricVehicle => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::SMALL_ELECTRIC_VEHICLE),

            // Motorized double-tracked vehicles
            Motorcar => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::MOTORCAR),
            Motorhome => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::MOTORHOME),
            TouristBus => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::TOURIST_BUS),
            Coach => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::COACH),
            Goods => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::GOODS),
            Hgv => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::GOODS)
                .union(TransportModeSet::HGV),
            HgvArticulated => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::GOODS)
                .union(TransportModeSet::HGV)
                .union(TransportModeSet::HGV_ARTICULATED),
            Bdouble => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::GOODS)
                .union(TransportModeSet::HGV)
                .union(TransportModeSet::BDOUBLE),
            Agricultural => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::AGRICULTURAL),
            AutoRickshaw => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::AUTO_RICKSHAW),
            Nev => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::NEV),
            GolfCart => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::GOLF_CART),
            Microcar => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::MICROCAR),
            Atv => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::ATV),
            Ohv => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::OHV),
            Snowmobile => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::SNOWMOBILE),

            // Vehicles by use/purpose (these often inherit from motor_vehicle or specific types)
            Psv => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::PSV),
            Bus => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::PSV)
                .union(TransportModeSet::BUS),
            Taxi => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::TAXI),
            Minibus => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::PSV)
                .union(TransportModeSet::BUS)
                .union(TransportModeSet::MINIBUS),
            ShareTaxi => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::TAXI)
                .union(TransportModeSet::SHARE_TAXI),
            Hov => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::HOV),
            Carpool => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::HOV)
                .union(TransportModeSet::CARPOOL),
            CarSharing => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::CAR_SHARING),
            Emergency => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::EMERGENCY),
            Hazmat => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::HAZMAT),
            HazmatWater => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::HAZMAT)
                .union(TransportModeSet::HAZMAT_WATER),
            SchoolBus => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::PSV)
                .union(TransportModeSet::BUS)
                .union(TransportModeSet::SCHOOL_BUS),
            Disabled => TransportModeSet::ALL
                .union(TransportModeSet::VEHICLE)
                .union(TransportModeSet::MOTOR_VEHICLE)
                .union(TransportModeSet::DISABLED),

            // === WATER TRANSPORT ===
            WaterAccess => TransportModeSet::WATER_ACCESS,
            Swimming => TransportModeSet::WATER_ACCESS.union(TransportModeSet::SWIMMING),
            IceSkates => TransportModeSet::WATER_ACCESS.union(TransportModeSet::ICE_SKATES),

            Boat => TransportModeSet::WATER_ACCESS.union(TransportModeSet::BOAT),
            Motorboat => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::MOTORBOAT),
            Sailboat => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SAILBOAT),
            Canoe => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::CANOE),
            FishingVessel => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::FISHING_VESSEL),

            Ship => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP),
            Passenger => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::PASSENGER),
            Cargo => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::CARGO),
            Bulk => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::CARGO)
                .union(TransportModeSet::BULK),
            Tanker => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::CARGO)
                .union(TransportModeSet::TANKER),
            TankerGas => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::CARGO)
                .union(TransportModeSet::TANKER)
                .union(TransportModeSet::TANKER_GAS),
            TankerOil => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::CARGO)
                .union(TransportModeSet::TANKER)
                .union(TransportModeSet::TANKER_OIL),
            TankerChemical => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::CARGO)
                .union(TransportModeSet::TANKER)
                .union(TransportModeSet::TANKER_CHEMICAL),
            TankerSinglehull => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::CARGO)
                .union(TransportModeSet::TANKER)
                .union(TransportModeSet::TANKER_SINGLEHULL),
            Container => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::CARGO)
                .union(TransportModeSet::CONTAINER),
            Imdg => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::IMDG),
            Isps => TransportModeSet::WATER_ACCESS
                .union(TransportModeSet::BOAT)
                .union(TransportModeSet::SHIP)
                .union(TransportModeSet::ISPS),

            // === RAIL TRANSPORT ===
            RailAccess => TransportModeSet::RAIL_ACCESS,
            Train => TransportModeSet::RAIL_ACCESS.union(TransportModeSet::TRAIN),
            Tram => TransportModeSet::RAIL_ACCESS.union(TransportModeSet::TRAM),
            Metro => TransportModeSet::RAIL_ACCESS.union(TransportModeSet::METRO),
        }
    }

    /// Check if this transport mode is affected by a restriction on the given mode
    #[inline]
    pub const fn is_restricted_by(&self, restriction_mode: TransportMode) -> bool {
        self.hierarchy_chain_flags()
            .contains(restriction_mode.to_flag())
    }

    /// Check if this transport mode matches any of the given restriction modes
    pub fn matches_any_restriction(&self, restrictions: &[TransportMode]) -> bool {
        let my_chain = self.hierarchy_chain();
        restrictions
            .iter()
            .any(|restriction| my_chain.contains(restriction))
    }

    /// Get all parent modes (excluding self)
    fn parent_modes(&self) -> &'static [TransportMode] {
        let chain = self.hierarchy_chain();
        if chain.len() > 1 {
            &chain[..chain.len() - 1]
        } else {
            &[]
        }
    }

    /// Check if this mode is a parent of another mode
    #[cfg(test)]
    pub fn is_parent_of(&self, other: TransportMode) -> bool {
        other.parent_modes().contains(self)
    }

    /// Get the immediate parent (most specific parent)
    pub fn immediate_parent(&self) -> Option<TransportMode> {
        let parents = self.parent_modes();
        parents.last().copied()
    }

    /// Check if this is a land-based transport mode
    pub fn is_land(&self) -> bool {
        matches!(
            self,
            Self::All
                | Self::Foot
                | Self::Dog
                | Self::Ski
                | Self::SkiNordic
                | Self::SkiAlpine
                | Self::SkiTelemark
                | Self::InlineSkates
                | Self::Horse
                | Self::Portage
                | Self::Bicycle
                | Self::ElectricBicycle
                | Self::Mtb
                | Self::CargoBike
                | Self::KickScooter
                | Self::Carriage
                | Self::CycleRickshaw
                | Self::HandCart
                | Self::Trailer
                | Self::Caravan
                | Self::Vehicle
                | Self::MotorVehicle
                | Self::Motorcycle
                | Self::Moped
                | Self::SpeedPedelec
                | Self::Mofa
                | Self::SmallElectricVehicle
                | Self::Motorcar
                | Self::Motorhome
                | Self::TouristBus
                | Self::Coach
                | Self::Goods
                | Self::Hgv
                | Self::HgvArticulated
                | Self::Bdouble
                | Self::Agricultural
                | Self::AutoRickshaw
                | Self::Nev
                | Self::GolfCart
                | Self::Microcar
                | Self::Atv
                | Self::Ohv
                | Self::Snowmobile
                | Self::Psv
                | Self::Bus
                | Self::Taxi
                | Self::Minibus
                | Self::ShareTaxi
                | Self::Hov
                | Self::Carpool
                | Self::CarSharing
                | Self::Emergency
                | Self::Hazmat
                | Self::HazmatWater
                | Self::SchoolBus
                | Self::Disabled
        )
    }

    /// Check if this is a water-based transport mode
    pub fn is_water(&self) -> bool {
        matches!(
            self,
            Self::WaterAccess
                | Self::Swimming
                | Self::IceSkates
                | Self::Boat
                | Self::Motorboat
                | Self::Sailboat
                | Self::Canoe
                | Self::FishingVessel
                | Self::Ship
                | Self::Passenger
                | Self::Cargo
                | Self::Bulk
                | Self::Tanker
                | Self::TankerGas
                | Self::TankerOil
                | Self::TankerChemical
                | Self::TankerSinglehull
                | Self::Container
                | Self::Imdg
                | Self::Isps
        )
    }

    /// Check if this is a rail-based transport mode
    pub fn is_rail(&self) -> bool {
        matches!(
            self,
            Self::RailAccess | Self::Train | Self::Tram | Self::Metro
        )
    }

    /// Check if this is a motorized vehicle
    pub fn is_motorized(&self) -> bool {
        matches!(
            self,
            Self::MotorVehicle
                | Self::Motorcycle
                | Self::Moped
                | Self::SpeedPedelec
                | Self::Mofa
                | Self::SmallElectricVehicle
                | Self::Motorcar
                | Self::Motorhome
                | Self::TouristBus
                | Self::Coach
                | Self::Goods
                | Self::Hgv
                | Self::HgvArticulated
                | Self::Bdouble
                | Self::Agricultural
                | Self::AutoRickshaw
                | Self::Nev
                | Self::GolfCart
                | Self::Microcar
                | Self::Atv
                | Self::Ohv
                | Self::Snowmobile
                | Self::Psv
                | Self::Bus
                | Self::Taxi
                | Self::Minibus
                | Self::ShareTaxi
                | Self::Emergency
                | Self::Motorboat
        )
    }

    /// Check if this is a non-motorized vehicle
    pub fn is_non_motorized(&self) -> bool {
        matches!(
            self,
            Self::Bicycle
                | Self::ElectricBicycle
                | Self::Mtb
                | Self::CargoBike
                | Self::KickScooter
                | Self::Carriage
                | Self::CycleRickshaw
                | Self::HandCart
                | Self::Trailer
                | Self::Caravan
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_parsing() {
        // Test some common transport modes
        assert_eq!(
            TransportMode::from_str("foot").unwrap(),
            TransportMode::Foot
        );
        assert_eq!(
            TransportMode::from_str("bicycle").unwrap(),
            TransportMode::Bicycle
        );
        assert_eq!(
            TransportMode::from_str("motorcar").unwrap(),
            TransportMode::Motorcar
        );
        assert_eq!(
            TransportMode::from_str("boat").unwrap(),
            TransportMode::Boat
        );
        assert_eq!(
            TransportMode::from_str("hazmat:water").unwrap(),
            TransportMode::HazmatWater
        );

        // Test serialization
        assert_eq!(TransportMode::Foot.to_string(), "foot");
        assert_eq!(TransportMode::HazmatWater.to_string(), "hazmat:water");
    }

    #[test]
    fn test_categorization() {
        assert!(TransportMode::Foot.is_land());
        assert!(TransportMode::Boat.is_water());
        assert!(TransportMode::Train.is_rail());

        assert!(TransportMode::Motorcar.is_motorized());
        assert!(TransportMode::Bicycle.is_non_motorized());

        assert!(!TransportMode::Foot.is_motorized());
        assert!(!TransportMode::Foot.is_non_motorized());
    }
}

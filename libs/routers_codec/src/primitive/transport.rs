pub struct VehicleCosting {
    pub height: f32,
    pub width: f32,
}

pub struct TruckCosting {
    pub vehicle_costing: VehicleCosting,

    pub length: f32,
    pub axle_load: f32,
    pub axle_count: u8,

    /// Is the truck carrying hazardous materials.
    /// This may exclude varius
    pub hazmat_load: bool,
}

/// A general set of transportation modes which can be used (agnostically)
/// to translate into the concrete map implementations.
pub enum TransportMode {
    Car(Option<VehicleCosting>),
    Bus(Option<VehicleCosting>),
    Truck(Option<TruckCosting>),
    Unspecified,
}

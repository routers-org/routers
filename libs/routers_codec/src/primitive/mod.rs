pub mod transport;

pub mod context {
    use crate::primitive::transport::TransportMode;

    pub struct TripContext {
        pub transport_mode: TransportMode,
    }
}

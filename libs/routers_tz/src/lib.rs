pub mod interface;
pub mod model;
pub mod storage;

pub(crate) mod generated {
    #[cfg(feature = "basic")]
    pub mod basic {
        include!(concat!(env!("OUT_DIR"), "/basic_timezone_storage.rs"));
    }

    #[cfg(feature = "rtree")]
    pub mod rtree {
        include!(concat!(env!("OUT_DIR"), "/rtree_timezone_storage.rs"));
    }

    #[cfg(feature = "s2cell")]
    pub mod s2cell {
        include!(concat!(env!("OUT_DIR"), "/s2cell_timezone_storage.rs"));
    }
}

// Trait Definition
pub use interface::TimezoneResolver;

// Timezone Type
pub use routers_tz_types::TimeZone;

// Basic Storage Impl
#[cfg(feature = "basic")]
pub use storage::basic::BasicStorage;

// RTree Storage Impl
#[cfg(feature = "rtree")]
pub use storage::rtree::RTreeStorage;

// S2Cell Storage Impl
#[cfg(feature = "s2cell")]
pub use storage::s2cell::S2CellStorage;

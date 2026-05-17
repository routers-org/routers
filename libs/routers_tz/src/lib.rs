pub mod interface;
pub mod model;
pub mod storage;

pub(crate) mod generated {
    //! Pre-baked backend data shipped with the crate. The bytes are produced
    //! by `build.rs` from the timezone geojson and committed into
    //! `data/prebuilt/`, so consumers never need the (very large) source
    //! geojson to build.

    // The `basic` backend's pre-built data is ~113 MiB (every timezone
    // polygon, serialised) and is not shipped in the published crate. It's
    // produced by `build.rs` when the source geojson is present locally; in
    // that case `build.rs` sets the `have_basic_prebuilt` cfg.
    #[cfg(all(feature = "basic", have_basic_prebuilt))]
    pub mod basic {
        use lazy_static::lazy_static;
        use routers_tz_types::storage::basic::BasicStorageBackend;

        const DATA: &[u8] =
            include_bytes!("../data/prebuilt/basic_timezone_data.postcard.bin");

        lazy_static! {
            pub static ref STORAGE: BasicStorageBackend = postcard::from_bytes(DATA)
                .expect("Failed to deserialize basic timezone data");
        }

        pub fn storage() -> &'static BasicStorageBackend {
            &STORAGE
        }
    }

    #[cfg(all(feature = "basic", not(have_basic_prebuilt)))]
    compile_error!(
        "The `basic` feature requires the timezone-boundary-builder geojson \
         at `data/<tz_version>/timezones.geojson` so `build.rs` can generate \
         `data/prebuilt/basic_timezone_data.postcard.bin`. The pre-built \
         binary is too large (~113 MiB) to ship via crates.io. Either disable \
         the `basic` feature (the default is `s2cell`), or build from a git \
         checkout with the geojson present."
    );

    #[cfg(feature = "rtree")]
    pub mod rtree {
        use lazy_static::lazy_static;
        use routers_tz_types::storage::rtree::RTreeStorageBackend;

        const DATA: &[u8] =
            include_bytes!("../data/prebuilt/rtree_timezone_data.postcard.bin");

        lazy_static! {
            pub static ref STORAGE: RTreeStorageBackend = postcard::from_bytes(DATA)
                .expect("Failed to deserialize rtree timezone data");
        }

        pub fn storage() -> &'static RTreeStorageBackend {
            &STORAGE
        }
    }

    #[cfg(feature = "s2cell")]
    pub mod s2cell {
        use lazy_static::lazy_static;
        use routers_tz_types::storage::s2cell::S2StorageBackend;

        const DATA: &[u8] =
            include_bytes!("../data/prebuilt/s2cell_timezone_data.postcard.bin");

        lazy_static! {
            pub static ref STORAGE: S2StorageBackend = postcard::from_bytes(DATA)
                .expect("Failed to deserialize s2cell timezone data");
        }

        pub fn storage() -> &'static S2StorageBackend {
            &STORAGE
        }
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

extern crate alloc;

#[allow(clippy::all)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/mvt.rs"));

    #[cfg(feature = "example")]
    include!(concat!(env!("OUT_DIR"), "/example.rs"));
}

pub mod datasource;
#[macro_use]
pub mod macros;
#[doc(hidden)]
pub mod error;
#[cfg(feature = "example")]
pub mod example;
#[doc(hidden)]
pub mod primitives;
pub mod query;

#[doc(inline)]
pub use datasource::query::Query;
#[doc(inline)]
pub use datasource::query::TileQuery;
#[doc(inline)]
pub use primitives::*;
#[doc(inline)]
pub use repository::RepositorySet;

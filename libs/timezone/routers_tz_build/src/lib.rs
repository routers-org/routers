#[cfg(feature = "basic")]
pub mod basic {
    include!(concat!(env!("OUT_DIR"), "/basic_timezone_storage.rs"));
}

#[cfg(feature = "rtree")]
pub mod rtree {
    include!(concat!(env!("OUT_DIR"), "/rtree_timezone_storage.rs"));
}

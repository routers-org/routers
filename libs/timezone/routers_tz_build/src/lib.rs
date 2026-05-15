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

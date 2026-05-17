//! Build script: extracts timezone geometries once, then hands them to each
//! enabled storage backend. Backend implementations live in `build/`.

pub type BoxError = Box<dyn std::error::Error>;

#[path = "build/codegen.rs"]
mod codegen;
#[path = "build/geojson.rs"]
mod geojson;

#[cfg(feature = "basic")]
#[path = "build/impl/basic.rs"]
mod basic;
#[cfg(feature = "rtree")]
#[path = "build/impl/rtree.rs"]
mod rtree;
#[cfg(feature = "s2cell")]
#[path = "build/impl/s2cell.rs"]
mod s2cell;

fn main() -> Result<(), BoxError> {
    let timezones = geojson::extract_timezones()?;
    eprintln!("Processed timezone polygons");

    #[cfg(feature = "basic")]
    basic::build(&timezones)?;
    #[cfg(feature = "rtree")]
    rtree::build(&timezones)?;
    #[cfg(feature = "s2cell")]
    s2cell::build(&timezones)?;

    eprintln!("Wrote data files");
    Ok(())
}

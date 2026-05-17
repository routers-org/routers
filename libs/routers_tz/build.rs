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
    // Custom cfg keys we may emit. Declaring them keeps `--check-cfg` happy
    // on toolchains that lint unknown cfgs.
    println!("cargo:rustc-check-cfg=cfg(have_basic_prebuilt)");

    let Some(timezones) = geojson::extract_timezones()? else {
        // Source geojson absent — published-crate build path. The pre-baked
        // s2cell/rtree binaries shipped in `data/prebuilt/` are sufficient.
        // The `basic` backend's prebuilt is ~113 MiB and is not shipped, so
        // leave `have_basic_prebuilt` unset and let lib.rs error out clearly
        // if the user tries to enable the `basic` feature without rebuilding
        // from source.
        eprintln!("Skipping timezone codegen (no source geojson present)");
        return Ok(());
    };
    eprintln!("Processed timezone polygons");

    #[cfg(feature = "basic")]
    {
        basic::build(&timezones)?;
        println!("cargo:rustc-cfg=have_basic_prebuilt");
    }
    #[cfg(feature = "rtree")]
    rtree::build(&timezones)?;
    #[cfg(feature = "s2cell")]
    s2cell::build(&timezones)?;

    eprintln!("Wrote data files");
    Ok(())
}

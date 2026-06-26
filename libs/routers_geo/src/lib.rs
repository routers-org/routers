pub const MVT_EXTENT: u32 = 4096;
pub const MVT_VERSION: u32 = 2;

pub const MEAN_EARTH_RADIUS: f64 = 6371008.8;
pub const SRID3857_MAX_LNG: u32 = 20026377;

pub mod cluster;
#[doc(hidden)]
pub mod coord;
#[doc(hidden)]
pub mod error;
pub mod project;

#[doc(inline)]
pub use coord::point::TileItem;
#[doc(inline)]
pub use project::Project;

#[macro_export]
macro_rules! dump_wkt {
    ($file:expr, $geom:expr) => {{
        #[cfg(debug_assertions)]
        {
            use std::fs::File;
            use std::io::Write;
            use wkt::ToWkt;

            let mut file = File::create($file).expect("Failed to create file");

            let wkt_str = $geom.to_wkt().to_string();
            file.write_all(wkt_str.as_bytes())
                .expect("Failed to write WKT to file");
        }
    }};
}

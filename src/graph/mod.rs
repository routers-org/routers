pub mod impls;
pub mod item;
pub mod traits;

pub use item::*;
pub use traits::*;

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

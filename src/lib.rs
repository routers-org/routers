#![doc = include_str!("../README.md")]
#![allow(dead_code)]

extern crate alloc;

pub use routers_transition::*;

pub mod transition {
    pub use routers_transition::*;
}

pub mod codec {
    pub use routers_codec::*;
}

pub mod network {
    pub use routers_network::*;
}

pub mod shard {
    pub use routers_shard::*;
}

#[cfg(test)]
pub mod test;

#[cfg(test)]
pub mod testing;

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

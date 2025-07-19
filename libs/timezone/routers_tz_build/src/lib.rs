pub mod data {
    // Include the generated timezone data
    include!(concat!(env!("OUT_DIR"), "/timezone_data.rs"));
}

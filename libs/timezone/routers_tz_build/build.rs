use geo::{Coord, LineString, MultiPolygon, Polygon};
use geojson::{GeoJson, Geometry, Value as GeoValue};
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use routers_tz_types::{BasicStorageBackend, BasicTimezone, IANATimezoneName, Timezone};

fn convert_geojson_to_geo_multipolygon(
    geometry: &Geometry,
) -> Result<MultiPolygon<f64>, Box<dyn std::error::Error>> {
    match &geometry.value {
        GeoValue::Polygon(polygon_coords) => {
            if polygon_coords.is_empty() {
                return Err("Empty polygon".into());
            }

            // Convert exterior ring
            let exterior_coords: Vec<Coord<f64>> = polygon_coords[0]
                .iter()
                .map(|coord| Coord {
                    x: coord[0],
                    y: coord[1],
                })
                .collect();
            let exterior = LineString::from(exterior_coords);

            // Convert interior rings
            let interiors: Vec<LineString<f64>> = polygon_coords[1..]
                .iter()
                .map(|ring| {
                    let coords: Vec<Coord<f64>> = ring
                        .iter()
                        .map(|coord| Coord {
                            x: coord[0],
                            y: coord[1],
                        })
                        .collect();
                    LineString::from(coords)
                })
                .collect();

            let polygon = Polygon::new(exterior, interiors);
            Ok(MultiPolygon::new(vec![polygon]))
        }
        GeoValue::MultiPolygon(multi_polygon_coords) => {
            let polygons: Result<Vec<Polygon<f64>>, Box<dyn std::error::Error>> =
                multi_polygon_coords
                    .iter()
                    .map(|polygon_coords| {
                        if polygon_coords.is_empty() {
                            return Err("Empty polygon in multipolygon".into());
                        }

                        // Convert exterior ring
                        let exterior_coords: Vec<Coord<f64>> = polygon_coords[0]
                            .iter()
                            .map(|coord| Coord {
                                x: coord[0],
                                y: coord[1],
                            })
                            .collect();
                        let exterior = LineString::from(exterior_coords);

                        // Convert interior rings
                        let interiors: Vec<LineString<f64>> = polygon_coords[1..]
                            .iter()
                            .map(|ring| {
                                let coords: Vec<Coord<f64>> = ring
                                    .iter()
                                    .map(|coord| Coord {
                                        x: coord[0],
                                        y: coord[1],
                                    })
                                    .collect();
                                LineString::from(coords)
                            })
                            .collect();

                        Ok(Polygon::new(exterior, interiors))
                    })
                    .collect();

            Ok(MultiPolygon::new(polygons?))
        }
        _ => Err("Unsupported geometry type".into()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let version = "2025b";

    let geojson_path = format!("data/{version}/timezones.geojson");
    println!("cargo:rerun-if-changed={geojson_path}");

    // Read the GeoJSON file
    let geojson_content = fs::read_to_string(geojson_path)?;
    let geojson: GeoJson = geojson_content.parse()?;

    let mut basic_timezones = Vec::new();

    match geojson {
        GeoJson::FeatureCollection(collection) => {
            for feature in collection.features {
                // Extract timezone ID from properties
                let tzid = feature
                    .properties
                    .as_ref()
                    .and_then(|props| props.get("tzid"))
                    .and_then(|v| v.as_str())
                    .ok_or("Missing or invalid tzid property")?
                    .to_string();

                // Create timezone structure
                let timezone = Timezone {
                    iana: IANATimezoneName(tzid),
                };

                // Convert geometry using geo crate
                let geometry = feature.geometry.as_ref().ok_or("Missing geometry")?;

                let multi_polygon = convert_geojson_to_geo_multipolygon(geometry)?;

                basic_timezones.push(BasicTimezone {
                    timezone,
                    geometry: multi_polygon,
                });
            }
        }
        _ => return Err("Expected FeatureCollection".into()),
    }

    let storage = BasicStorageBackend {
        polygons: basic_timezones,
    };

    // Write to output directory
    let out_dir = env::var("OUT_DIR")?;
    let dest_path = Path::new(&out_dir).join("timezone_data.bin");
    let mut file = File::create(&dest_path)?;

    // Serialize with bincode
    bincode::encode_into_std_write(storage, &mut file, bincode::config::standard())?;

    // Generate Rust code to include the data
    let code_path = Path::new(&out_dir).join("timezone_data.rs");
    let mut code_file = File::create(&code_path)?;

    writeln!(
        code_file,
        r#"
// Code generated by @bennjii 2025
use lazy_static::lazy_static;
use routers_tz_types::BasicStorageBackend;
use bincode;

const TIMEZONE_DATA: &[u8] = include_bytes!("timezone_data.bin");

lazy_static! {{
    pub static ref TIMEZONE_STORAGE: BasicStorageBackend = {{
        bincode::decode_from_slice(TIMEZONE_DATA, bincode::config::standard())
            .expect("Failed to deserialize timezone data")
            .0
    }};
}}

pub fn get_timezone_storage() -> &'static BasicStorageBackend {{
    &*TIMEZONE_STORAGE
}}
"#
    )?;

    println!("Processed timezone polygons");
    Ok(())
}

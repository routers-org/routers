use geo::{Coord, LineString, MultiPolygon, Polygon};
use geojson::{GeoJson, Geometry, Value as GeoValue};
use routers_tz_types::storage::basic::BasicStorageBackend;
use routers_tz_types::storage::rtree::EncodableRTreeStorageBackend;
use routers_tz_types::timezone::internal::{TimeZoneGeometry, TimeZoneName, TimezoneBuild};
use serde::Serialize;
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

#[cfg(feature = "s2cell")]
use routers_tz_types::storage::s2cell::S2StorageBackend;

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

fn extract_timezones() -> Result<Vec<TimezoneBuild>, Box<dyn std::error::Error>> {
    let version = "2026a";

    let geojson_path = format!("data/{version}/timezones.geojson");
    println!("cargo:rerun-if-changed={geojson_path}");

    // Read the GeoJSON file
    let geojson_content = fs::read_to_string(geojson_path)?;
    let geojson: GeoJson = geojson_content.parse()?;

    let mut timezones = Vec::new();

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

                // Convert geometry using geo crate
                let geometry = feature.geometry.ok_or("Missing geometry")?;
                let multi_polygon = convert_geojson_to_geo_multipolygon(&geometry)?;

                if let Some(timezone_ref) = time_tz::timezones::get_by_name(tzid.as_str()) {
                    // Create timezone structure
                    timezones.push(TimezoneBuild {
                        tz: timezone_ref,
                        name: TimeZoneName::new(tzid),
                        geometry: TimeZoneGeometry(multi_polygon),
                    });
                } else {
                    eprintln!("Could not find timezone '{tzid}' in the database");
                }
            }
        }
        _ => return Err("Expected FeatureCollection".into()),
    }

    Ok(timezones)
}

fn rtree_backend(timezones: &[TimezoneBuild]) -> Result<(), Box<dyn std::error::Error>> {
    let backend = EncodableRTreeStorageBackend::new(timezones);
    write_backend(backend, "rtree", "RTreeStorageBackend")
}

fn basic_backend(constructs: &[TimezoneBuild]) -> Result<(), Box<dyn std::error::Error>> {
    let mut names = vec![];
    let mut geometries = vec![];

    constructs
        .iter()
        .for_each(|TimezoneBuild { name, geometry, .. }| {
            names.push(name.clone());
            geometries.push(geometry.clone());
        });

    let backend = BasicStorageBackend { geometries, names };

    write_backend(backend, "basic", "BasicStorageBackend")
}

fn write_backend(
    backend: impl Serialize,
    dir: &str,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let data_binary = format!("{dir}_timezone_data.bin");
    let codegen_file = format!("{dir}_timezone_storage.rs");

    // Write to output directory
    let out_dir = env::var("OUT_DIR")?;
    let dest_path = Path::new(&out_dir).join(data_binary.clone());
    let mut file = File::create(&dest_path)?;

    let output: Vec<u8> =
        postcard::to_allocvec(&backend).map_err(|e| format!("failed to serialise value: {e}"))?;

    file.write_all(&output).map_err(|e| e.to_string())?;

    // Generate Rust code to include the data
    let code_path = Path::new(&out_dir).join(codegen_file);
    let mut code_file = File::create(&code_path)?;

    eprintln!("Writing to {code_path:?}");

    writeln!(
        code_file,
        r#"
// Code generated by @bennjii 2025
use lazy_static::lazy_static;
use routers_tz_types::storage::{dir}::*;

const TIMEZONE_DATA: &[u8] = include_bytes!("{data_binary}");

lazy_static! {{
    pub static ref STORAGE: {name} = {{
        postcard::from_bytes(TIMEZONE_DATA)
            .expect("Failed to deserialize {dir} timezone data")
    }};
}}

pub fn storage() -> &'static {name} {{
    &STORAGE
}}
"#
    )?;

    Ok(())
}

// MultiPolygonRegion caches the overall S2 bounding rect plus per-sub-polygon bounding
// rects so we can skip sub-polygons cheaply before calling geo::Contains.
#[cfg(feature = "s2cell")]
#[derive(Clone)]
struct MultiPolygonRegion {
    poly: geo::MultiPolygon<f64>,
    sub_rects: Vec<s2::rect::Rect>,
    s2_rect: s2::rect::Rect,
}

#[cfg(feature = "s2cell")]
impl MultiPolygonRegion {
    fn new(poly: geo::MultiPolygon<f64>) -> Self {
        use geo::BoundingRect;

        let make_rect = |b: geo::Rect<f64>| {
            s2::rect::Rect::from_point_pair(
                &s2::latlng::LatLng::from_degrees(b.min().y, b.min().x),
                &s2::latlng::LatLng::from_degrees(b.max().y, b.max().x),
            )
        };

        let sub_rects: Vec<s2::rect::Rect> = poly
            .0
            .iter()
            .map(|p| {
                p.bounding_rect()
                    .map(make_rect)
                    .unwrap_or_else(s2::rect::Rect::empty)
            })
            .collect();

        let s2_rect = poly
            .bounding_rect()
            .map(make_rect)
            .unwrap_or_else(s2::rect::Rect::empty);

        MultiPolygonRegion {
            poly,
            sub_rects,
            s2_rect,
        }
    }

    fn poly_contains_point(&self, pt: &geo::Point<f64>, cell_rect: &s2::rect::Rect) -> bool {
        use geo::Contains;
        self.poly
            .0
            .iter()
            .zip(&self.sub_rects)
            .any(|(polygon, sub_rect)| sub_rect.intersects(cell_rect) && polygon.contains(pt))
    }
}

#[cfg(feature = "s2cell")]
impl s2::region::Region for MultiPolygonRegion {
    fn cap_bound(&self) -> s2::cap::Cap {
        s2::cap::Cap::full()
    }

    fn rect_bound(&self) -> s2::rect::Rect {
        self.s2_rect.clone()
    }

    fn contains_cell(&self, c: &s2::cell::Cell) -> bool {
        use s2::latlng::LatLng;

        if !self.s2_rect.intersects(&c.rect_bound()) {
            return false;
        }

        let cell_rect = c.rect_bound();
        c.vertices().iter().all(|v| {
            let ll = LatLng::from(v);
            self.poly_contains_point(&geo::Point::new(ll.lng.deg(), ll.lat.deg()), &cell_rect)
        })
    }

    fn intersects_cell(&self, c: &s2::cell::Cell) -> bool {
        use s2::latlng::LatLng;

        if !self.s2_rect.intersects(&c.rect_bound()) {
            return false;
        }

        let cell_rect = c.rect_bound();

        // Any cell vertex inside the polygon?
        if c.vertices().iter().any(|v| {
            let ll = LatLng::from(v);
            self.poly_contains_point(&geo::Point::new(ll.lng.deg(), ll.lat.deg()), &cell_rect)
        }) {
            return true;
        }

        // Any polygon vertex inside the cell? This replaces the old bbox-vs-bbox fallback,
        // which was too coarse: Germany's bbox covers all of Switzerland, causing Germany to
        // generate cells deep inside Swiss territory. Checking actual polygon vertices avoids
        // false positives — a cell far inside Switzerland has no German polygon vertices.
        self.poly
            .0
            .iter()
            .zip(&self.sub_rects)
            .any(|(polygon, sub_rect)| {
                if !sub_rect.intersects(&cell_rect) {
                    return false;
                }
                polygon.exterior().points().any(|pt| {
                    let ll = LatLng::from_degrees(pt.y(), pt.x());
                    let pt_rect = s2::rect::Rect::from_point_pair(&ll, &ll);
                    cell_rect.intersects(&pt_rect)
                })
            })
    }
}

#[cfg(feature = "s2cell")]
fn s2cell_backend(timezones: &[TimezoneBuild]) -> Result<(), Box<dyn std::error::Error>> {
    use rayon::prelude::*;
    use s2::region::RegionCoverer;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MIN_LEVEL: u8 = 1;
    const MAX_LEVEL: u8 = 13;
    const MAX_CELLS: usize = 1000;

    // Cache filename encodes all covering parameters so any change auto-invalidates.
    let cache_path = format!("data/s2cell_l{MAX_LEVEL}_c{MAX_CELLS}.postcard.bin");
    println!("cargo:rerun-if-changed={cache_path}");

    let out_dir = env::var("OUT_DIR")?;
    let dest_path = Path::new(&out_dir).join("s2cell_timezone_data.bin");

    // Use the cache if it exists and is at least as new as the GeoJSON source data.
    let geojson_mtime = fs::metadata("data/2026a/timezones.geojson")
        .and_then(|m| m.modified())
        .ok();
    let cache_fresh = Path::new(&cache_path).exists()
        && geojson_mtime.map_or(false, |gj| {
            fs::metadata(&cache_path)
                .and_then(|m| m.modified())
                .map_or(false, |ct| ct >= gj)
        });

    if cache_fresh {
        eprintln!("[s2cell] using cached covering from {cache_path}");
        fs::copy(&cache_path, &dest_path)?;
    } else {
        let total = timezones.len();
        eprintln!(
            "[s2cell] computing covering for {total} timezones (l{MIN_LEVEL}–{MAX_LEVEL}, max {MAX_CELLS} cells each)"
        );

        let done = AtomicUsize::new(0);

        // Process timezones in parallel; each produces its own (cell_id, tz_idx) pairs.
        let all_cells: Vec<(u64, u32)> = timezones
            .par_iter()
            .enumerate()
            .flat_map(|(tz_idx, tz)| {
                let region = MultiPolygonRegion::new(tz.geometry.0.clone());
                let coverer = RegionCoverer {
                    min_level: MIN_LEVEL,
                    max_level: MAX_LEVEL,
                    level_mod: 1,
                    max_cells: MAX_CELLS,
                };
                let covering = coverer.covering(&region);
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n % 10 == 0 || n == total {
                    eprintln!("[s2cell] {n}/{total}");
                }
                covering
                    .0
                    .into_iter()
                    .map(move |cell_id| (cell_id.0, tz_idx as u32))
                    .collect::<Vec<_>>()
            })
            .collect();

        eprintln!(
            "[s2cell] resolving {} cells across {total} timezones",
            all_cells.len()
        );

        // Resolve cell-ownership conflicts deterministically: when multiple timezones
        // generate the same cell ID (at a shared boundary), assign it to the timezone
        // whose polygon actually contains the cell's centre point.  If neither contains
        // the centre (a true straddle cell), the first entry encountered wins.
        let mut cell_map: std::collections::HashMap<u64, u32> =
            std::collections::HashMap::with_capacity(all_cells.len());

        for (cell_id, tz_idx) in &all_cells {
            cell_map
                .entry(*cell_id)
                .and_modify(|existing_tz| {
                    use geo::Contains;
                    use s2::latlng::LatLng;
                    let cell = s2::cell::Cell::from(s2::cellid::CellID(*cell_id));
                    let ll = LatLng::from(&cell.center());
                    let pt = geo::Point::new(ll.lng.deg(), ll.lat.deg());
                    let new_contains = timezones[*tz_idx as usize].geometry.0.contains(&pt);
                    let old_contains = timezones[*existing_tz as usize].geometry.0.contains(&pt);
                    if new_contains && !old_contains {
                        *existing_tz = *tz_idx;
                    }
                })
                .or_insert(*tz_idx);
        }

        let mut cell_pairs: Vec<(u64, u32)> = cell_map.into_iter().collect();
        cell_pairs.sort_unstable_by_key(|(id, _)| *id);

        let mut cell_ids = Vec::with_capacity(cell_pairs.len());
        let mut tz_indices = Vec::with_capacity(cell_pairs.len());
        for (id, idx) in cell_pairs {
            cell_ids.push(id);
            tz_indices.push(idx);
        }

        eprintln!(
            "[s2cell] {} unique cells across {total} timezones",
            cell_ids.len()
        );

        let names: Vec<_> = timezones.iter().map(|tz| tz.name.clone()).collect();
        let backend = S2StorageBackend {
            cell_ids,
            tz_indices,
            names,
        };

        // Write to OUT_DIR and save a copy to the source-tree cache.
        let output = postcard::to_allocvec(&backend)
            .map_err(|e| format!("failed to serialise s2cell backend: {e}"))?;
        let mut file = File::create(&dest_path)?;
        file.write_all(&output).map_err(|e| e.to_string())?;
        drop(file);

        fs::copy(&dest_path, &cache_path)?;
        eprintln!("[s2cell] cache saved to {cache_path}");
    }

    // Codegen is always cheap — write the .rs include regardless of cache hit.
    let codegen_file = "s2cell_timezone_storage.rs";
    let code_path = Path::new(&out_dir).join(codegen_file);
    let mut code_file = File::create(&code_path)?;
    let (dir, name) = ("s2cell", "S2StorageBackend");
    let data_binary = "s2cell_timezone_data.bin";
    writeln!(
        code_file,
        r#"
// Code generated by @bennjii 2025
use lazy_static::lazy_static;
use routers_tz_types::storage::{dir}::*;

const TIMEZONE_DATA: &[u8] = include_bytes!("{data_binary}");

lazy_static! {{
    pub static ref STORAGE: {name} = {{
        postcard::from_bytes(TIMEZONE_DATA)
            .expect("Failed to deserialize {dir} timezone data")
    }};
}}

pub fn storage() -> &'static {name} {{
    &STORAGE
}}
"#
    )?;

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let timezones = extract_timezones()?;
    eprintln!("Processed timezone polygons");

    // For all backends, if the feature is enabled - generate it.
    if env::var("CARGO_FEATURE_RTREE").is_ok() {
        rtree_backend(&timezones)?;
    }

    if env::var("CARGO_FEATURE_BASIC").is_ok() {
        basic_backend(&timezones)?;
    }

    #[cfg(feature = "s2cell")]
    if env::var("CARGO_FEATURE_S2CELL").is_ok() {
        s2cell_backend(&timezones)?;
    }

    eprintln!("Wrote data files");
    Ok(())
}

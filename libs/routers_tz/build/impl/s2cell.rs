use rayon::prelude::*;
use routers_tz_types::storage::s2cell::S2StorageBackend;
use routers_tz_types::timezone::internal::TimezoneBuild;
use s2::region::RegionCoverer;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::BoxError;
use crate::codegen::Backend;
use crate::geojson::geojson_path;

const MIN_LEVEL: u8 = 1;
const MAX_LEVEL: u8 = 13;
const MAX_CELLS: usize = 1000;

pub fn build(timezones: &[TimezoneBuild]) -> Result<(), BoxError> {
    // Cache filename encodes covering parameters so any change auto-invalidates.
    let cache_path = format!("data/s2cell_l{MAX_LEVEL}_c{MAX_CELLS}.postcard.bin");
    println!("cargo:rerun-if-changed={cache_path}");

    let backend = Backend {
        module: "s2cell",
        type_name: "S2StorageBackend",
    };
    let dest_path = backend.data_path()?;

    if cache_is_fresh(&cache_path) {
        eprintln!("[s2cell] using cached covering from {cache_path}");
        fs::copy(&cache_path, &dest_path)?;
    } else {
        backend.write_data(compute_backend(timezones))?;
        fs::copy(&dest_path, &cache_path)?;
        eprintln!("[s2cell] cache saved to {cache_path}");
    }

    backend.write_codegen()
}

fn cache_is_fresh(cache_path: &str) -> bool {
    let geojson_mtime = fs::metadata(geojson_path())
        .and_then(|m| m.modified())
        .ok();
    Path::new(cache_path).exists()
        && geojson_mtime.is_some_and(|gj| {
            fs::metadata(cache_path)
                .and_then(|m| m.modified())
                .is_ok_and(|ct| ct >= gj)
        })
}

fn compute_backend(timezones: &[TimezoneBuild]) -> S2StorageBackend {
    let total = timezones.len();
    eprintln!(
        "[s2cell] computing covering for {total} timezones (l{MIN_LEVEL}–{MAX_LEVEL}, max {MAX_CELLS} cells each)"
    );

    let all_cells = cover_all(timezones);
    eprintln!(
        "[s2cell] resolving {} cells across {total} timezones",
        all_cells.len()
    );

    let (cell_ids, tz_indices) = resolve_conflicts(all_cells, timezones);
    eprintln!(
        "[s2cell] {} unique cells across {total} timezones",
        cell_ids.len()
    );

    S2StorageBackend {
        cell_ids,
        tz_indices,
        names: timezones.iter().map(|tz| tz.name.clone()).collect(),
    }
}

/// Compute the S2 cell covering for every timezone in parallel.
fn cover_all(timezones: &[TimezoneBuild]) -> Vec<(u64, u32)> {
    let total = timezones.len();
    let done = AtomicUsize::new(0);

    timezones
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
        .collect()
}

/// Resolve cell-ownership conflicts deterministically. When multiple
/// timezones generate the same cell ID (at a shared boundary), assign it
/// to the timezone whose polygon actually contains the cell's centre point.
/// On a true straddle cell where neither contains the centre, the first
/// entry encountered wins.
fn resolve_conflicts(
    all_cells: Vec<(u64, u32)>,
    timezones: &[TimezoneBuild],
) -> (Vec<u64>, Vec<u32>) {
    use geo::Contains;
    use s2::latlng::LatLng;

    let mut cell_map: HashMap<u64, u32> = HashMap::with_capacity(all_cells.len());

    for (cell_id, tz_idx) in &all_cells {
        cell_map
            .entry(*cell_id)
            .and_modify(|existing| {
                let cell = s2::cell::Cell::from(s2::cellid::CellID(*cell_id));
                let ll = LatLng::from(&cell.center());
                let pt = geo::Point::new(ll.lng.deg(), ll.lat.deg());
                let new_contains = timezones[*tz_idx as usize].geometry.0.contains(&pt);
                let old_contains = timezones[*existing as usize].geometry.0.contains(&pt);
                if new_contains && !old_contains {
                    *existing = *tz_idx;
                }
            })
            .or_insert(*tz_idx);
    }

    let mut pairs: Vec<(u64, u32)> = cell_map.into_iter().collect();
    pairs.sort_unstable_by_key(|(id, _)| *id);
    pairs.into_iter().unzip()
}

/// An `s2::region::Region` view over a `geo::MultiPolygon`.
///
/// Caches the overall and per-sub-polygon S2 bounding rects so we can skip
/// sub-polygons cheaply before falling back to the (expensive) point-in-polygon
/// test via `geo::Contains`.
struct MultiPolygonRegion {
    poly: geo::MultiPolygon<f64>,
    sub_rects: Vec<s2::rect::Rect>,
    s2_rect: s2::rect::Rect,
}

impl MultiPolygonRegion {
    fn new(poly: geo::MultiPolygon<f64>) -> Self {
        use geo::BoundingRect;

        let make_rect = |b: geo::Rect<f64>| {
            s2::rect::Rect::from_point_pair(
                &s2::latlng::LatLng::from_degrees(b.min().y, b.min().x),
                &s2::latlng::LatLng::from_degrees(b.max().y, b.max().x),
            )
        };

        let sub_rects = poly
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

        // Any polygon vertex inside the cell? This replaces a naive bbox-vs-bbox
        // fallback that was too coarse: Germany's bbox covers all of Switzerland,
        // causing Germany to generate cells deep inside Swiss territory. Checking
        // actual polygon vertices avoids those false positives.
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

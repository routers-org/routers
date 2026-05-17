use geo::{Coord, LineString, MultiPolygon, Polygon};
use geojson::{GeoJson, Geometry, Value as GeoValue};
use routers_tz_types::timezone::internal::{TimeZoneGeometry, TimeZoneName, TimezoneBuild};
use std::fs;

use crate::BoxError;

/// Default IANA tzdata release used when `ROUTERS_TZ_VERSION` is unset.
/// Bumping this is the one-line update for a new tzdata release that has
/// already been vendored into `data/`.
const DEFAULT_TZ_VERSION: &str = "2026a";

/// Resolve the tzdata version to build against. Overridable at build time
/// via the `ROUTERS_TZ_VERSION` environment variable (e.g.
/// `ROUTERS_TZ_VERSION=2026b cargo build`).
pub fn tz_version() -> String {
    if std::env::var_os("ROUTERS_TZ_VERSION").is_some() {
        println!("cargo:rerun-if-env-changed=ROUTERS_TZ_VERSION");
    }
    std::env::var("ROUTERS_TZ_VERSION").unwrap_or_else(|_| DEFAULT_TZ_VERSION.to_string())
}

pub fn geojson_path() -> String {
    format!("data/{}/timezones.geojson", tz_version())
}

pub fn extract_timezones() -> Result<Option<Vec<TimezoneBuild>>, BoxError> {
    let path = geojson_path();
    if !std::path::Path::new(&path).exists() {
        // No source geojson present (e.g. published crate using pre-baked
        // artifacts). Skip extraction entirely and don't emit a rerun hint
        // for a path that will never exist in this build tree.
        return Ok(None);
    }
    println!("cargo:rerun-if-changed={path}");

    let geojson: GeoJson = fs::read_to_string(&path)?.parse()?;

    let GeoJson::FeatureCollection(collection) = geojson else {
        return Err("Expected FeatureCollection".into());
    };

    let mut timezones = Vec::with_capacity(collection.features.len());

    for feature in collection.features {
        let tzid = feature
            .properties
            .as_ref()
            .and_then(|props| props.get("tzid"))
            .and_then(|v| v.as_str())
            .ok_or("Missing or invalid tzid property")?
            .to_string();

        let geometry = feature.geometry.ok_or("Missing geometry")?;
        let multi_polygon = to_multi_polygon(&geometry)?;

        let Some(tz) = time_tz::timezones::get_by_name(tzid.as_str()) else {
            eprintln!("Could not find timezone '{tzid}' in the database");
            continue;
        };

        timezones.push(TimezoneBuild {
            tz,
            name: TimeZoneName::new(tzid),
            geometry: TimeZoneGeometry(multi_polygon),
        });
    }

    Ok(Some(timezones))
}

fn to_multi_polygon(geometry: &Geometry) -> Result<MultiPolygon<f64>, BoxError> {
    match &geometry.value {
        GeoValue::Polygon(rings) => Ok(MultiPolygon::new(vec![rings_to_polygon(rings)?])),
        GeoValue::MultiPolygon(polys) => polys
            .iter()
            .map(|rings| rings_to_polygon(rings))
            .collect::<Result<Vec<_>, _>>()
            .map(MultiPolygon::new),
        _ => Err("Unsupported geometry type".into()),
    }
}

fn rings_to_polygon(rings: &[Vec<Vec<f64>>]) -> Result<Polygon<f64>, BoxError> {
    let [exterior, interiors @ ..] = rings else {
        return Err("Empty polygon".into());
    };

    Ok(Polygon::new(
        ring_to_linestring(exterior),
        interiors.iter().map(|r| ring_to_linestring(r)).collect(),
    ))
}

fn ring_to_linestring(ring: &[Vec<f64>]) -> LineString<f64> {
    LineString::from(
        ring.iter()
            .map(|c| Coord { x: c[0], y: c[1] })
            .collect::<Vec<_>>(),
    )
}

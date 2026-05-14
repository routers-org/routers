use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;

use super::GpsTrace;

/// Minimal GeoJSON Feature deserialiser — only the fields we need.
#[derive(Deserialize)]
struct Feature {
    properties: Properties,
    geometry: Geometry,
}

#[derive(Deserialize)]
struct Properties {
    id: String,
}

#[derive(Deserialize)]
struct Geometry {
    #[serde(rename = "type")]
    kind: String,
    coordinates: Vec<[f64; 2]>,
}

/// Load a GeoJSON Feature file from `path` and return a [GpsTrace].
///
/// The file must contain a single GeoJSON Feature with a LineString geometry.
/// Each coordinate element is `[longitude, latitude]` per the GeoJSON spec.
pub fn load(path: &Path) -> Result<GpsTrace> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading trace file {}", path.display()))?;

    let feature: Feature = serde_json::from_str(&raw)
        .with_context(|| format!("parsing GeoJSON from {}", path.display()))?;

    if feature.geometry.kind != "LineString" {
        bail!(
            "{}: expected LineString geometry, got {}",
            path.display(),
            feature.geometry.kind
        );
    }

    let points = feature
        .geometry
        .coordinates
        .into_iter()
        .map(|c| (c[0], c[1]))
        .collect();

    Ok(GpsTrace {
        id: feature.properties.id,
        points,
    })
}

use anyhow::{Context, Result};
use geo::LineString;
use serde_json::{json, to_string_pretty};
use std::path::Path;
use wkt::TryFromWkt;

use routers_fixtures::{LAX_LYNWOOD_TRIP, SYNDEY_TRIP, VENTURA_TRIP};

struct TraceSource {
    id: &'static str,
    wkt: &'static str,
}

const SOURCES: &[TraceSource] = &[
    TraceSource {
        id: "ventura",
        wkt: VENTURA_TRIP,
    },
    TraceSource {
        id: "lax_lynwood",
        wkt: LAX_LYNWOOD_TRIP,
    },
    TraceSource {
        id: "sydney",
        wkt: SYNDEY_TRIP,
    },
];

/// Convert the bundled WKT fixture strings into GeoJSON files under `out_dir`.
pub fn init_traces(out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    for src in SOURCES {
        let ls: LineString<f64> = LineString::try_from_wkt_str(src.wkt)
            .map_err(|e| anyhow::anyhow!("WKT parse error for {}: {e:?}", src.id))?;

        let coords: Vec<[f64; 2]> = ls.coords().map(|c| [c.x, c.y]).collect();

        let feature = json!({
            "type": "Feature",
            "properties": {
                "id": src.id,
                "point_count": coords.len()
            },
            "geometry": {
                "type": "LineString",
                "coordinates": coords
            }
        });

        let path = out_dir.join(format!("{}.geojson", src.id));
        std::fs::write(&path, to_string_pretty(&feature)?)
            .with_context(|| format!("writing {}", path.display()))?;

        println!("  wrote {} ({} points)", path.display(), coords.len());
    }

    Ok(())
}

use std::time::Instant;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;

use crate::config::GraphHopperConfig;
use crate::matcher::{MatchResult, Matcher};
use crate::trace::GpsTrace;

pub struct GraphHopperMatcher {
    client: Client,
    base_url: String,
    profile: String,
    gps_accuracy: u32,
}

impl GraphHopperMatcher {
    pub fn new(cfg: &GraphHopperConfig) -> Self {
        Self {
            client: Client::new(),
            base_url: cfg.url.trim_end_matches('/').to_string(),
            profile: cfg.profile.clone(),
            gps_accuracy: cfg.gps_accuracy,
        }
    }
}

impl Matcher for GraphHopperMatcher {
    fn name(&self) -> &str { "graphhopper" }

    fn health_check(&self) -> anyhow::Result<()> {
        self.client
            .get(format!("{}/info", self.base_url))
            .send()
            .with_context(|| {
                format!(
                    "GraphHopper is not reachable at {}. \
                     Start it with: just conform::graphhopper",
                    self.base_url
                )
            })?
            .error_for_status()
            .with_context(|| "GraphHopper /info returned an error")?;
        Ok(())
    }

    /// POST to `/match` with a GPX XML body.
    ///
    /// GraphHopper's map-matching API accepts GPX as its primary input format.
    /// Query parameters (profile, gps_accuracy) must go in the URL; the body
    /// is raw XML with Content-Type `application/gpx+xml`.
    ///
    /// Using `points_encoded=false` to get plain coordinate arrays avoids a
    /// decoding step and keeps the response format straightforward.
    fn match_trace(&self, trace: &GpsTrace) -> Result<MatchResult> {
        let gpx = build_gpx(&trace.id, &trace.points);

        let url = format!(
            "{}/match?profile={}&gps_accuracy={}&points_encoded=false",
            self.base_url, self.profile, self.gps_accuracy
        );

        let t0 = Instant::now();
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/gpx+xml")
            .body(gpx)
            .send()
            .with_context(|| format!("GraphHopper request to {url}"))?;
        let duration = t0.elapsed();

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            bail!("GraphHopper returned HTTP {status}: {text}");
        }

        Ok(MatchResult { point_count: trace.point_count(), duration })
    }
}

/// Synthesise a minimal GPX 1.1 document from a sequence of (lon, lat) points.
///
/// GraphHopper requires `<trkpt>` elements with `lat`/`lon` attributes.
/// Timestamps are omitted — GraphHopper does not require them for map matching.
fn build_gpx(trace_id: &str, points: &[(f64, f64)]) -> String {
    let mut gpx = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="routers-conformance"
     xmlns="http://www.topografix.com/GPX/1/1">
  <trk><name>"#,
    );
    gpx.push_str(trace_id);
    gpx.push_str("</name><trkseg>\n");

    for &(lon, lat) in points {
        gpx.push_str(&format!(
            r#"    <trkpt lat="{lat:.8}" lon="{lon:.8}"/>"#
        ));
        gpx.push('\n');
    }

    gpx.push_str("  </trkseg></trk>\n</gpx>");
    gpx
}

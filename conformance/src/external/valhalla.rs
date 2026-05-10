use std::time::Instant;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::config::ValhallaConfig;
use crate::matcher::{MatchResult, Matcher};
use crate::trace::GpsTrace;

pub struct ValhallaMatcher {
    client: Client,
    url: String,
    costing: String,
    shape_match: String,
    gps_accuracy: u32,
}

impl ValhallaMatcher {
    pub fn new(cfg: &ValhallaConfig) -> Self {
        Self {
            client: Client::new(),
            url: format!("{}/trace_route", cfg.url.trim_end_matches('/')),
            costing: cfg.costing.clone(),
            shape_match: cfg.shape_match.clone(),
            gps_accuracy: cfg.gps_accuracy,
        }
    }
}

impl Matcher for ValhallaMatcher {
    fn name(&self) -> &str { "valhalla" }

    /// POST to `/trace_route` with Valhalla's standard JSON format.
    ///
    /// The full round-trip (serialise → TCP → deserialise) is timed so that
    /// the measurement reflects real client-perceived latency, not just
    /// server-side computation.  This is consistent with how all HTTP matchers
    /// in this suite are measured.
    fn match_trace(&self, trace: &GpsTrace) -> Result<MatchResult> {
        let shape: Vec<Value> = trace
            .points
            .iter()
            .map(|&(lon, lat)| json!({ "lat": lat, "lon": lon }))
            .collect();

        let body = json!({
            "shape": shape,
            "costing": self.costing,
            "shape_match": self.shape_match,
            "gps_accuracy": self.gps_accuracy,
        });

        let t0 = Instant::now();
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .with_context(|| format!("Valhalla request to {}", self.url))?;
        let duration = t0.elapsed();

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            bail!("Valhalla returned HTTP {status}: {text}");
        }

        Ok(MatchResult { point_count: trace.point_count(), duration })
    }
}

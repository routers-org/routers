use std::time::Instant;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::json;

use crate::config::FmmConfig;
use crate::matcher::{MatchResult, Matcher};
use crate::trace::GpsTrace;

pub struct FmmMatcher {
    client: Client,
    url: String,
    k: u32,
    radius: f64,
    error: f64,
}

impl FmmMatcher {
    pub fn new(cfg: &FmmConfig) -> Self {
        Self {
            client: Client::new(),
            url: format!("{}/match", cfg.url.trim_end_matches('/')),
            k: cfg.k,
            radius: cfg.radius,
            error: cfg.error,
        }
    }
}

impl Matcher for FmmMatcher {
    fn name(&self) -> &str { "fmm" }

    fn health_check(&self) -> anyhow::Result<()> {
        let base = self.url.trim_end_matches("/match");
        self.client
            .get(format!("{base}/health"))
            .send()
            .with_context(|| {
                format!(
                    "FMM server is not reachable at {base}. \
                     Start it with: nix run .#start-fmm"
                )
            })?
            .error_for_status()
            .with_context(|| "FMM /health returned an error")?;
        Ok(())
    }

    /// POST to the FMM C++ HTTP server with a JSON body.
    ///
    /// The server (`fmm_server/main.cpp`) wraps the FMM C++ library with a
    /// minimal cpp-httplib endpoint.  It accepts GPS points as a JSON array of
    /// [lon, lat] pairs and returns the matched path synchronously.
    ///
    /// Parameters (k, radius, error) are passed per-request so the server can
    /// be shared across different benchmark configurations without restart.
    fn match_trace(&self, trace: &GpsTrace) -> Result<MatchResult> {
        let points: Vec<[f64; 2]> = trace
            .points
            .iter()
            .map(|&(lon, lat)| [lon, lat])
            .collect();

        let body = json!({
            "points": points,
            "k": self.k,
            "radius": self.radius,
            "error": self.error,
        });

        let t0 = Instant::now();
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .with_context(|| format!("FMM request to {}", self.url))?;
        let duration = t0.elapsed();

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            bail!("FMM server returned HTTP {status}: {text}");
        }

        Ok(MatchResult { point_count: trace.point_count(), duration })
    }
}

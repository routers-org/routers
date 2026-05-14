pub mod routers;

use std::time::Duration;

use anyhow::Result;

use crate::trace::GpsTrace;

/// The result of a single map-matching call.
#[derive(Debug, Clone)]
pub struct MatchResult {
    /// Number of GPS points in the input trace.
    pub point_count: usize,
    /// Wall-clock time of the matching call (network round-trip for HTTP matchers).
    pub duration: Duration,
}

/// Shared interface for all map matchers under test.
pub trait Matcher: Send + Sync {
    fn name(&self) -> &str;
    /// Run a single map-match operation and return timing + metadata.
    fn match_trace(&self, trace: &GpsTrace) -> Result<MatchResult>;
    /// Optional pre-benchmark readiness check.  Default is a no-op; HTTP
    /// matchers override this to verify the service is reachable before the
    /// timed run begins.
    fn health_check(&self) -> Result<()> {
        Ok(())
    }
}

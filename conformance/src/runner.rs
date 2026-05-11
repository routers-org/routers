use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;

use crate::matcher::Matcher;
use crate::metrics::MatcherMetrics;
use crate::trace::GpsTrace;

pub struct ConformanceRunner<'a> {
    pub matchers: Vec<Box<dyn Matcher>>,
    pub traces: &'a [GpsTrace],
    pub iterations: usize,
    pub warmup: usize,
    pub network_label: String,
}

impl<'a> ConformanceRunner<'a> {
    /// Run the full suite and return per-(matcher, trace) [MatcherMetrics].
    ///
    /// The map key is `"matcher / trace_id"` so each trace appears as its own
    /// row in the results table.  This avoids bimodal distributions caused by
    /// mixing short and long traces in a single percentile calculation.
    pub fn run(&self) -> Result<BTreeMap<String, MatcherMetrics>> {
        let mut out = BTreeMap::new();

        for matcher in &self.matchers {
            eprintln!("[{}] checking service readiness…", matcher.name());
            matcher.health_check()?;

            eprintln!(
                "[{}] warming up ({} pass(es))…",
                matcher.name(),
                self.warmup
            );
            self.warmup_matcher(matcher.as_ref())?;

            eprintln!(
                "[{}] timing {} iteration(s) × {} trace(s)…",
                matcher.name(),
                self.iterations,
                self.traces.len()
            );

            for trace in self.traces {
                let (samples, pts) = self.time_trace(matcher.as_ref(), trace)?;
                let metrics = MatcherMetrics::compute(samples, pts);
                eprintln!(
                    "[{}] done '{}' — {}",
                    matcher.name(),
                    trace.id,
                    crate::metrics::fmt_throughput(metrics.throughput_pts_per_sec),
                );
                let key = format!("{} / {} / {}", self.network_label, matcher.name(), trace.id);
                out.insert(key, metrics);
            }
        }

        Ok(out)
    }

    fn warmup_matcher(&self, matcher: &dyn Matcher) -> Result<()> {
        for _ in 0..self.warmup {
            for trace in self.traces {
                let _ = matcher.match_trace(trace)?;
            }
        }
        Ok(())
    }

    fn time_trace(
        &self,
        matcher: &dyn Matcher,
        trace: &GpsTrace,
    ) -> Result<(Vec<Duration>, usize)> {
        let mut samples = Vec::with_capacity(self.iterations);
        let mut total_points = 0usize;

        for _ in 0..self.iterations {
            let result = matcher.match_trace(trace)?;
            samples.push(result.duration);
            total_points += result.point_count;
        }

        Ok((samples, total_points))
    }
}

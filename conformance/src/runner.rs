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
}

impl<'a> ConformanceRunner<'a> {
    /// Run the full suite and return per-matcher [MatcherMetrics].
    pub fn run(&self) -> Result<BTreeMap<String, MatcherMetrics>> {
        let mut out = BTreeMap::new();

        for matcher in &self.matchers {
            eprintln!("[{}] checking service readiness…", matcher.name());
            matcher.health_check()?;

            eprintln!("[{}] warming up ({} pass(es))…", matcher.name(), self.warmup);
            self.warmup_matcher(matcher.as_ref())?;

            eprintln!(
                "[{}] timing {} iteration(s) × {} trace(s)…",
                matcher.name(),
                self.iterations,
                self.traces.len()
            );

            let (samples, total_points) = self.time_matcher(matcher.as_ref())?;
            let metrics = MatcherMetrics::compute(samples, total_points);

            eprintln!(
                "[{}] done — {:.2} pts/s",
                matcher.name(),
                metrics.throughput_pts_per_sec
            );

            out.insert(matcher.name().to_string(), metrics);
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

    fn time_matcher(&self, matcher: &dyn Matcher) -> Result<(Vec<Duration>, usize)> {
        let total_calls = self.iterations * self.traces.len();
        let mut samples = Vec::with_capacity(total_calls);
        let mut total_points = 0usize;

        for _ in 0..self.iterations {
            for trace in self.traces {
                let result = matcher.match_trace(trace)?;
                samples.push(result.duration);
                total_points += result.point_count;
            }
        }

        Ok((samples, total_points))
    }
}

use std::time::Duration;

/// Per-matcher statistical summary collected over all timed iterations.
#[derive(Debug, Clone)]
pub struct MatcherMetrics {
    /// Total GPS points matched across all iterations and traces.
    pub total_points: usize,
    /// Total wall time spent matching (denominator for throughput).
    pub total_duration: Duration,
    /// Points matched per second.
    pub throughput_pts_per_sec: f64,

    pub mean:   Duration,
    pub median: Duration,
    pub p15:    Duration,
    /// Lower quartile (25th percentile).
    pub lq:     Duration,
    /// Upper quartile (75th percentile).
    pub uq:     Duration,
    pub p85:    Duration,
    pub min:    Duration,
    pub max:    Duration,
}

impl MatcherMetrics {
    /// Compute metrics from a sorted or unsorted list of per-call durations.
    ///
    /// `total_points` is the sum of GPS points across every timed call so that
    /// throughput is computed as `total_points / total_wall_seconds`.
    pub fn compute(mut samples: Vec<Duration>, total_points: usize) -> Self {
        assert!(!samples.is_empty(), "cannot compute metrics on an empty sample set");

        samples.sort_unstable();

        let n = samples.len();
        let total_duration: Duration = samples.iter().sum();
        let total_secs = total_duration.as_secs_f64();

        let mean = total_duration / n as u32;
        let throughput = if total_secs > 0.0 {
            total_points as f64 / total_secs
        } else {
            f64::INFINITY
        };

        Self {
            total_points,
            total_duration,
            throughput_pts_per_sec: throughput,
            mean,
            median: percentile(&samples, 50),
            p15:    percentile(&samples, 15),
            lq:     percentile(&samples, 25),
            uq:     percentile(&samples, 75),
            p85:    percentile(&samples, 85),
            min:    samples[0],
            max:    samples[n - 1],
        }
    }
}

/// Nearest-rank percentile on a *sorted* slice.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
    let n = sorted.len();
    // ceil(p/100 * n) - 1, clamped to [0, n-1].
    let idx = (((p as f64 / 100.0) * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[idx]
}

/// Human-readable duration string, scaled to the most appropriate unit.
pub fn fmt_duration(d: Duration) -> String {
    let micros = d.as_micros();
    if micros < 1_000 {
        format!("{micros} µs")
    } else if micros < 1_000_000 {
        format!("{:.2} ms", micros as f64 / 1_000.0)
    } else {
        format!("{:.3} s", d.as_secs_f64())
    }
}

/// Compact throughput string: "1,234,567 pts/s".
pub fn fmt_throughput(pts_per_sec: f64) -> String {
    if pts_per_sec.is_infinite() {
        return "∞ pts/s".to_string();
    }
    let v = pts_per_sec as u64;
    // Insert thousands separators manually (no extra dep).
    let s = v.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 { out.push(','); }
        out.push(ch);
    }
    out.chars().rev().collect::<String>() + " pts/s"
}

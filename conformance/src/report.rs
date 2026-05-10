use std::collections::BTreeMap;

use serde_json::{Value, json};
use tabled::{
    Table, Tabled,
    settings::{Alignment, Style, object::Columns},
};

use crate::metrics::{MatcherMetrics, fmt_duration, fmt_throughput};

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "Matcher")]
    matcher: String,
    #[tabled(rename = "Throughput")]
    throughput: String,
    #[tabled(rename = "Mean")]
    mean: String,
    #[tabled(rename = "Median")]
    median: String,
    #[tabled(rename = "P15")]
    p15: String,
    #[tabled(rename = "LQ (P25)")]
    lq: String,
    #[tabled(rename = "UQ (P75)")]
    uq: String,
    #[tabled(rename = "P85")]
    p85: String,
    #[tabled(rename = "Min")]
    min: String,
    #[tabled(rename = "Max")]
    max: String,
}

impl Row {
    fn from(matcher: &str, m: &MatcherMetrics) -> Self {
        Self {
            matcher: matcher.to_string(),
            throughput: fmt_throughput(m.throughput_pts_per_sec),
            mean:   fmt_duration(m.mean),
            median: fmt_duration(m.median),
            p15:    fmt_duration(m.p15),
            lq:     fmt_duration(m.lq),
            uq:     fmt_duration(m.uq),
            p85:    fmt_duration(m.p85),
            min:    fmt_duration(m.min),
            max:    fmt_duration(m.max),
        }
    }
}

/// Print results as a terminal table.
pub fn print_table(results: &BTreeMap<String, MatcherMetrics>, iterations: usize, trace_count: usize) {
    println!(
        "\nConformance Results — {} iterations × {} trace(s)\n",
        iterations, trace_count
    );

    let rows: Vec<Row> = results
        .iter()
        .map(|(name, m)| Row::from(name, m))
        .collect();

    let mut table = Table::new(&rows);
    table
        .with(Style::modern())
        .modify(Columns::new(1..), Alignment::right());

    println!("{table}");
}

/// Serialise results to a JSON string.
pub fn to_json(results: &BTreeMap<String, MatcherMetrics>) -> String {
    let map: Value = results
        .iter()
        .map(|(name, m)| {
            (
                name.clone(),
                json!({
                    "throughput_pts_per_sec": m.throughput_pts_per_sec,
                    "mean_us":   m.mean.as_micros(),
                    "median_us": m.median.as_micros(),
                    "p15_us":    m.p15.as_micros(),
                    "lq_us":     m.lq.as_micros(),
                    "uq_us":     m.uq.as_micros(),
                    "p85_us":    m.p85.as_micros(),
                    "min_us":    m.min.as_micros(),
                    "max_us":    m.max.as_micros(),
                    "total_points": m.total_points,
                    "total_duration_us": m.total_duration.as_micros(),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();

    serde_json::to_string_pretty(&map).expect("serialisation cannot fail")
}

/// Serialise results to CSV.
pub fn to_csv(results: &BTreeMap<String, MatcherMetrics>) -> String {
    let mut out = String::from(
        "matcher,throughput_pts_per_sec,mean_us,median_us,p15_us,lq_us,uq_us,p85_us,min_us,max_us\n"
    );
    for (name, m) in results {
        out.push_str(&format!(
            "{},{:.2},{},{},{},{},{},{},{},{}\n",
            name,
            m.throughput_pts_per_sec,
            m.mean.as_micros(),
            m.median.as_micros(),
            m.p15.as_micros(),
            m.lq.as_micros(),
            m.uq.as_micros(),
            m.p85.as_micros(),
            m.min.as_micros(),
            m.max.as_micros(),
        ));
    }
    out
}

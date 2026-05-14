mod config;
mod external;
mod init;
mod matcher;
mod metrics;
mod report;
mod runner;
mod trace;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use config::Config;
use matcher::Matcher;
use runner::ConformanceRunner;
use trace::loader;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Subcommand dispatch — no clap needed at this scale.
    match args.get(1).map(|s| s.as_str()) {
        Some("init-traces") => {
            let out_dir = PathBuf::from("data/traces");
            println!("Initialising trace files in {}…", out_dir.display());
            init::init_traces(&out_dir)?;
            println!("Done.");
            return Ok(());
        }
        // merge <file1.json> [file2.json ...] — combine per-matcher JSON results
        // into a single terminal table.  Later files override earlier ones for
        // the same matcher name (so the most comprehensive run wins).
        Some("merge") => {
            let files = &args[2..];
            if files.is_empty() {
                eprintln!("Usage: routers-conformance merge <results1.json> [results2.json ...]");
                std::process::exit(1);
            }
            let mut combined = std::collections::BTreeMap::new();
            for path in files {
                let json =
                    std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
                let parsed: serde_json::Value = serde_json::from_str(&json)
                    .with_context(|| format!("parsing JSON from {path}"))?;
                if let Some(obj) = parsed.as_object() {
                    for (name, v) in obj {
                        combined.insert(name.clone(), metrics_from_json(v));
                    }
                }
            }
            report::print_merged_table(&combined);
            return Ok(());
        }
        Some(arg) if arg.starts_with('-') && arg != "--output" => {
            eprintln!("Usage: routers-conformance [--output json|csv|table] [config.toml]");
            eprintln!("       routers-conformance init-traces");
            eprintln!("       routers-conformance merge <results1.json> [results2.json ...]");
            std::process::exit(1);
        }
        _ => {}
    }

    // Parse optional --output <format> flag anywhere in the remaining args.
    let mut output_override: Option<String> = None;
    let mut positional: Vec<&str> = Vec::new();
    let mut i = 1usize;
    while i < args.len() {
        if args[i] == "--output" {
            output_override = args.get(i + 1).cloned();
            i += 2;
        } else {
            positional.push(&args[i]);
            i += 1;
        }
    }

    // Optional config path as first positional argument.
    let config_path: PathBuf = positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("conformance.toml"));

    let cfg = load_config(&config_path)?;

    // Build the list of active matchers.
    let matchers = build_matchers(&cfg)?;
    if matchers.is_empty() {
        eprintln!("No matchers enabled — set `enabled = true` in conformance.toml");
        std::process::exit(1);
    }

    // Load traces.
    let traces = load_traces(&cfg)?;
    if traces.is_empty() {
        eprintln!("No traces found — run `routers-conformance init-traces` first");
        std::process::exit(1);
    }

    let network_label = cfg
        .matchers
        .routers
        .as_ref()
        .filter(|r| r.enabled)
        .map(|r| network_label_from_path(&r.network))
        .unwrap_or_else(|| "network".to_string());

    let runner = ConformanceRunner {
        matchers,
        traces: &traces,
        iterations: cfg.run.iterations,
        warmup: cfg.run.warmup,
        network_label,
    };

    let results = runner.run()?;

    let output = output_override.as_deref().unwrap_or(&cfg.run.output);
    match output {
        "json" => println!("{}", report::to_json(&results)),
        "csv" => print!("{}", report::to_csv(&results)),
        _ => report::print_table(&results, cfg.run.iterations, traces.len()),
    }

    Ok(())
}

/// Reconstruct a `MatcherMetrics` from the JSON format emitted by `to_json`.
fn metrics_from_json(v: &serde_json::Value) -> metrics::MatcherMetrics {
    use std::time::Duration;
    let us = |key: &str| Duration::from_micros(v[key].as_u64().unwrap_or(0));
    metrics::MatcherMetrics {
        total_points: v["total_points"].as_u64().unwrap_or(0) as usize,
        total_duration: us("total_duration_us"),
        throughput_pts_per_sec: v["throughput_pts_per_sec"].as_f64().unwrap_or(0.0),
        mean: us("mean_us"),
        median: us("median_us"),
        p15: us("p15_us"),
        lq: us("lq_us"),
        uq: us("uq_us"),
        p85: us("p85_us"),
        min: us("min_us"),
        max: us("max_us"),
    }
}

fn network_label_from_path(network: &str) -> String {
    let name = Path::new(network)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(network);
    let name = name.strip_suffix(".osm.pbf").unwrap_or(name);
    name.strip_suffix("-minified").unwrap_or(name).to_string()
}

fn load_config(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config from {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing TOML config from {}", path.display()))
}

fn build_matchers(cfg: &Config) -> Result<Vec<Box<dyn Matcher>>> {
    use external::{fmm::FmmMatcher, graphhopper::GraphHopperMatcher, valhalla::ValhallaMatcher};
    use matcher::routers::RoutersMatcher;

    let mut matchers: Vec<Box<dyn Matcher>> = Vec::new();

    if let Some(r) = &cfg.matchers.routers {
        if r.enabled {
            eprintln!("Loading Routers network (this may take a moment)…");
            matchers.push(Box::new(RoutersMatcher::new(r)?));
        }
    }
    if let Some(v) = &cfg.matchers.valhalla {
        if v.enabled {
            matchers.push(Box::new(ValhallaMatcher::new(v)));
        }
    }
    if let Some(g) = &cfg.matchers.graphhopper {
        if g.enabled {
            matchers.push(Box::new(GraphHopperMatcher::new(g)));
        }
    }
    if let Some(f) = &cfg.matchers.fmm {
        if f.enabled {
            matchers.push(Box::new(FmmMatcher::new(f)));
        }
    }

    Ok(matchers)
}

fn load_traces(cfg: &Config) -> Result<Vec<trace::GpsTrace>> {
    let mut traces = Vec::new();
    for entry in &cfg.traces {
        let path = PathBuf::from(&entry.file);
        let trace = loader::load(&path)
            .with_context(|| format!("loading trace '{}' from {}", entry.id, path.display()))?;
        traces.push(trace);
    }
    Ok(traces)
}

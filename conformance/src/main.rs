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
        Some(arg) if arg.starts_with('-') => {
            eprintln!("Usage: routers-conformance [config.toml]");
            eprintln!("       routers-conformance init-traces");
            std::process::exit(1);
        }
        _ => {}
    }

    // Optional config path as first (non-subcommand) argument.
    let config_path: PathBuf = args
        .get(1)
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

    let runner = ConformanceRunner {
        matchers,
        traces: &traces,
        iterations: cfg.run.iterations,
        warmup: cfg.run.warmup,
    };

    let results = runner.run()?;

    match cfg.run.output.as_str() {
        "json" => println!("{}", report::to_json(&results)),
        "csv"  => print!("{}", report::to_csv(&results)),
        _      => report::print_table(&results, cfg.run.iterations, traces.len()),
    }

    Ok(())
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

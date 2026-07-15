//! Visual diff for benchmark snapshot changes.
//!
//! Compares `benches/snapshots/*_coords.snap` in the working tree against a
//! base git ref (default: `merge-base(main, HEAD)`) and renders both matched
//! geometries on side-by-side synced map panes. See `.claude/SPEC.md`.
//!
//! ```sh
//! gh pr checkout 183
//! cargo run -p routers_viewer --bin snapdiff
//! cargo run -p routers_viewer --bin snapdiff -- --base origin/main
//! ```

#![warn(clippy::all, rust_2018_idioms)]

mod app;
mod diff;
mod git;
mod parse;

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context as _, Result};

use crate::app::SnapDiffApp;
use crate::diff::FixtureDiff;
use crate::parse::{Snapshot, parse_coords_snap};

const COORDS_SUFFIX: &str = "_coords.snap";
const USAGE: &str = "\
snapdiff — visual diff of benchmark coordinate snapshots against a base ref

USAGE:
    snapdiff [--base <ref>] [--no-merge-base] [--snapshots <dir>]

OPTIONS:
    --base <ref>        Base git ref to compare against [default: main]
    --no-merge-base     Compare against the ref's tip, not merge-base(<ref>, HEAD)
    --snapshots <dir>   Snapshot dir relative to the repo root [default: benches/snapshots]
";

struct Args {
    base: String,
    merge_base: bool,
    snapshots: String,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        base: "main".to_owned(),
        merge_base: true,
        snapshots: "benches/snapshots".to_owned(),
    };

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--base" => args.base = iter.next().context("--base requires a ref")?,
            "--no-merge-base" => args.merge_base = false,
            "--snapshots" => args.snapshots = iter.next().context("--snapshots requires a dir")?,
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    Ok(args)
}

/// `map_match__LAX_LYNWOOD_coords.snap` → `LAX_LYNWOOD`.
fn fixture_name(file: &str) -> String {
    let stem = file.strip_suffix(COORDS_SUFFIX).unwrap_or(file);
    match stem.split_once("__") {
        Some((_, name)) => name.to_owned(),
        None => stem.to_owned(),
    }
}

fn parse_side(name: &str, side: &str, content: Option<String>) -> Result<Option<Snapshot>> {
    content
        .map(|c| parse_coords_snap(&c).with_context(|| format!("{name} ({side})")))
        .transpose()
}

fn load(args: &Args) -> Result<(String, Vec<FixtureDiff>)> {
    let root = git::repo_root()?;
    let sha = git::resolve_base(&root, &args.base, args.merge_base)?;
    let base_label = format!("{} @ {}", args.base, &sha[..sha.len().min(9)]);

    // Union of fixtures at the base commit and in the working tree, so both
    // added and removed fixtures are listed.
    let mut files: BTreeSet<String> = git::list_files_at(&root, &sha, &args.snapshots)?
        .into_iter()
        .filter_map(|p| Some(PathBuf::from(p).file_name()?.to_string_lossy().into_owned()))
        .collect();

    let snapshot_dir = root.join(&args.snapshots);
    if let Ok(entries) = std::fs::read_dir(&snapshot_dir) {
        for entry in entries.flatten() {
            files.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }

    files.retain(|f| f.ends_with(COORDS_SUFFIX));
    anyhow::ensure!(
        !files.is_empty(),
        "no *_coords.snap fixtures found under {}",
        snapshot_dir.display()
    );

    let fixtures = files
        .into_iter()
        .map(|file| {
            let name = fixture_name(&file);
            let rel_path = format!("{}/{}", args.snapshots, file);

            let base_txt = git::read_file_at(&root, &sha, &rel_path)?;
            let head_txt = match std::fs::read_to_string(snapshot_dir.join(&file)) {
                Ok(content) => Some(content),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e).context(rel_path),
            };

            let base = parse_side(&name, "base", base_txt);
            let head = parse_side(&name, "head", head_txt);

            Ok(match (base, head) {
                (Ok(base), Ok(head)) => FixtureDiff::compute(name, base, head),
                (Err(e), _) | (_, Err(e)) => FixtureDiff::parse_error(name, format!("{e:#}")),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((base_label, fixtures))
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args = parse_args()?;
    let (base_label, fixtures) = load(&args)?;

    let changed = fixtures.iter().filter(|f| f.status.changed()).count();
    log::info!(
        "{} fixtures, {changed} changed (base: {base_label})",
        fixtures.len()
    );

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("routers — snapshot diff")
            .with_inner_size([1500.0, 950.0])
            .with_min_inner_size([600.0, 400.0]),
        ..Default::default()
    };

    // Share the "routers" storage namespace so the Mapbox API key configured
    // in the main viewer applies here too.
    eframe::run_native(
        "routers",
        native_options,
        Box::new(move |ctx| Ok(Box::new(SnapDiffApp::new(ctx, base_label, fixtures)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_working_tree_against_main() {
        // CI checkouts are a shallow detached HEAD with no `main` ref (local
        // or remote-tracking) — this test only makes sense on a dev checkout.
        let can_resolve = git::repo_root()
            .and_then(|root| git::resolve_base(&root, "main", true))
            .is_ok();
        if !can_resolve {
            eprintln!("skipping: no resolvable `main`/`origin/main` in this checkout");
            return;
        }

        let args = Args {
            base: "main".to_owned(),
            merge_base: true,
            snapshots: "benches/snapshots".to_owned(),
        };
        let (label, fixtures) = load(&args).unwrap();
        assert!(label.starts_with("main @ "), "{label}");
        assert!(!fixtures.is_empty());
        for f in &fixtures {
            println!(
                "{} {:?} Δ{:.1}m +{}/−{}",
                f.name, f.status, f.magnitude_m, f.points_added, f.points_removed
            );
        }
        assert!(fixtures.iter().all(|f| f.error.is_none()));
    }

    #[test]
    fn fixture_names_strip_bench_prefix_and_suffix() {
        assert_eq!(
            fixture_name("map_match__LAX_LYNWOOD_coords.snap"),
            "LAX_LYNWOOD"
        );
        assert_eq!(
            fixture_name("map_match__VENTURA_HWY_coords.snap"),
            "VENTURA_HWY"
        );
        assert_eq!(fixture_name("plain_coords.snap"), "plain");
    }
}

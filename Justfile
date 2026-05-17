mod tz "libs/routers_tz"

init VERSION="2026a":
    just tz download {{ VERSION }}

# Run benchmarks. Writes updated snapshots to .snap.new instead of panicking,
# so all scenarios complete even when heuristics change.
# Run benchmarks. Updates snapshots in place so all scenarios complete even
# when heuristics change. Review changes afterwards with `just bench-review`.
bench:
    git lfs pull --include="benches/snapshots/*" --exclude=""
    INSTA_UPDATE=always cargo bench

# Review snapshot changes after `just bench` via git diff.
bench-review:
    git diff benches/snapshots/

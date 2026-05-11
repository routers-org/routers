mod tz "libs/routers_tz"

init VERSION="2026a":
    just tz download {{ VERSION }}
<<<<<<< HEAD

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
=======
# Run benchmarks. Writes updated snapshots to .snap.new instead of panicking,
# so all scenarios complete even when heuristics change.
bench:
    INSTA_UPDATE=new cargo bench

# Interactively review pending snapshot changes after `just bench`.
# Shows a side-by-side diff for each changed snapshot; accept or reject each one.
bench-review:
    cargo insta review
>>>>>>> 29756cd (fix(changelog): update snapshots, and costing fn)

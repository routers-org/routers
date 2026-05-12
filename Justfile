# Run benchmarks. Updates snapshots in place so all scenarios complete even
# when heuristics change. Review changes afterwards with `just bench-review`.
bench:
    INSTA_UPDATE=always cargo bench

# Review snapshot changes after `just bench` via git diff.
bench-review:
    git diff benches/snapshots/

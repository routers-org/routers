# Run benchmarks. Writes updated snapshots to .snap.new instead of panicking,
# so all scenarios complete even when heuristics change.
bench:
    INSTA_UPDATE=new cargo bench

# Interactively review pending snapshot changes after `just bench`.
# Shows a side-by-side diff for each changed snapshot; accept or reject each one.
bench-review:
    cargo insta review

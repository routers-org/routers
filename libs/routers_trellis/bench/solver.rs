use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use routers_trellis::*;

fn build(layers: usize, width: usize, seed: u64) -> Trellis {
    let mut t = Trellis::new(vec![width; layers]).unwrap();
    let mut s = seed | 1;
    let mut rng = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((s >> 40) as u32) % 256
    };
    for layer in 0..layers - 1 {
        let row: Vec<u32> = (0..width * width).map(|_| rng()).collect();
        t.fill_transition(layer, &row).unwrap();
    }
    t
}

/// Single-solve throughput across a range of (layers, width) sizes.
fn bench_single_solve(c: &mut Criterion) {
    let mut group = c.benchmark_group("viterbi/single");

    for &(l, w) in &[(10usize, 30usize), (16, 64), (64, 128), (256, 256)] {
        let t = build(l, w, 0xABCD);
        let mut solver = ViterbiSolver::new();

        group.bench_function(BenchmarkId::new("solve", format!("L{l}W{w}")), |b| {
            b.iter(|| solver.solve(black_box(&t)))
        });
    }

    group.finish();
}

/// Batch throughput: 1 000 independent solves distributed across 1–8 slots.
///
/// Graphs are built once before the benchmark loop so only the solve time
/// (including thread spawn and work-steal overhead) is measured.
fn bench_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("viterbi/batch");

    let graphs: Vec<Trellis> = (0..1_000).map(|k| build(10, 30, k as u64)).collect();

    for &slots in &[1usize, 2, 4, 8] {
        group.bench_function(BenchmarkId::new("slots", slots), |b| {
            b.iter(|| solve_batch(black_box(&graphs), slots))
        });
    }

    group.finish();
}

/// Solver warm-up: cost of the first solve when buffers are cold (un-allocated).
///
/// Uses `iter_batched` so each iteration starts with a fresh `ViterbiSolver`.
fn bench_cold_start(c: &mut Criterion) {
    let mut group = c.benchmark_group("viterbi/cold");

    for &(l, w) in &[(10usize, 30usize), (64, 128)] {
        let t = build(l, w, 0xABCD);

        group.bench_function(BenchmarkId::new("cold_start", format!("L{l}W{w}")), |b| {
            b.iter_batched(
                ViterbiSolver::new,
                |mut solver| solver.solve(black_box(&t)),
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

criterion_group!(benches, bench_single_solve, bench_batch, bench_cold_start);
criterion_main!(benches);

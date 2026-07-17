use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use routers_trellis::*;

fn build(layers: usize, width: u32, seed: u64) -> Trellis {
    let mut t = Trellis::new(vec![width; layers]).unwrap();
    let mut s = seed | 1;
    let mut rng = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((s >> 40) as u32) % 256
    };
    for layer in 0..layers - 1 {
        let row: Vec<u32> = (0..(width * width) as usize).map(|_| rng()).collect();
        t.fill_transition(LayerId(layer as u32), &row).unwrap();
    }
    t
}

fn solve_batch(trellises: &[Trellis], slots: usize) -> Vec<Result<Path, SolveError>> {
    let n = trellises.len();
    let mut out: Vec<Result<Path, SolveError>> =
        (0..n).map(|_| Ok(Path::new(Vec::new(), 0))).collect();

    if n == 0 {
        return out;
    }

    let slots = slots.max(1).min(n);
    let chunk = n.div_ceil(slots);

    std::thread::scope(|s| {
        for (tin, tout) in trellises.chunks(chunk).zip(out.chunks_mut(chunk)) {
            s.spawn(move || {
                let solver = ViterbiSolver::new();
                for (k, t) in tin.iter().enumerate() {
                    tout[k] = solver.solve(t);
                }
            });
        }
    });

    out
}

/// Single-solve throughput across a range of (layers, width) sizes.
fn bench_single_solve(c: &mut Criterion) {
    let mut group = c.benchmark_group("viterbi/single");

    for &(l, w) in &[(10usize, 30u32), (16, 64), (64, 128), (256, 256)] {
        let t = build(l, w, 0xABCD);
        let solver = ViterbiSolver::new();

        group.bench_function(BenchmarkId::new("solve", format!("L{l}W{w}")), |b| {
            b.iter(|| solver.solve(black_box(&t)))
        });
    }

    group.finish();
}

/// Batch throughput: 1 000 independent solves distributed across 1–8 slots.
fn bench_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("viterbi/batch");

    let graphs: Vec<Trellis> = (0..1_000u64).map(|k| build(10, 30, k)).collect();

    for &slots in &[1usize, 2, 4, 8] {
        group.bench_function(BenchmarkId::new("slots", slots), |b| {
            b.iter(|| solve_batch(black_box(&graphs), slots))
        });
    }

    group.finish();
}

criterion_group!(benches, bench_single_solve, bench_batch);
criterion_main!(benches);

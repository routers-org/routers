use routers_trellis::*;
use std::time::Instant;

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

fn bench<T>(label: &str, iters: u32, mut f: impl FnMut() -> T) {
    let _ = f();
    let start = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(f());
    }
    let ns = start.elapsed().as_nanos() as f64 / iters as f64;
    println!("  {label:<14} {:>11.3} us/solve", ns / 1000.0);
}

pub fn solve_batch(trellises: &[Trellis], slots: usize) -> Vec<Result<Path, SolveError>> {
    let n = trellises.len();
    let mut out: Vec<Result<Path, SolveError>> = (0..n)
        .map(|_| Ok(Path::new(Vec::new(), 0, false)))
        .collect();

    if n == 0 {
        return out;
    }

    let slots = slots.max(1).min(n);
    let chunk = n.div_ceil(slots);

    std::thread::scope(|s| {
        for (tin, tout) in trellises.chunks(chunk).zip(out.chunks_mut(chunk)) {
            s.spawn(move || {
                let mut solver = ViterbiSolver::new();
                for (k, t) in tin.iter().enumerate() {
                    tout[k] = solver.solve(t);
                }
            });
        }
    });

    out
}

fn main() {
    println!("single solve (one Viterbi slot):");
    for &(l, w) in &[(10usize, 30usize), (16, 64), (64, 128), (256, 256)] {
        let t = build(l, w, 0xABCD);
        let mut solver = ViterbiSolver::new();
        let iters = (20_000_000 / (l * w * w) as u32).max(50);
        bench(&format!("L={l} W={w}"), iters, || solver.solve(&t));
    }

    println!("\nbatch throughput (slots scale across cores):");
    let graphs: Vec<Trellis> = (0..10_000).map(|k| build(10, 30, k as u64)).collect();
    for &slots in &[1usize, 2, 4, 8] {
        let start = Instant::now();
        let out = solve_batch(&graphs, slots);
        let secs = start.elapsed().as_secs_f64();
        std::hint::black_box(&out);
        println!(
            "  slots={slots:<2} {:>11.0} solves/s ({:.2} ms total)",
            graphs.len() as f64 / secs,
            secs * 1000.0,
        );
    }
}

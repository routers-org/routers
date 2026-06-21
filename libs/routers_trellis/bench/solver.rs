use std::time::Instant;
use trellis::*;

fn build(l: usize, w: usize, seed: u64) -> Trellis {
    let mut t = Trellis::new(vec![w; l]).unwrap();
    let mut s = seed | 1;
    let mut rng = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((s >> 40) as u32) % 256
    };
    for layer in 0..l - 1 {
        let row: Vec<u32> = (0..w * w).map(|_| rng()).collect();
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

fn main() {
    println!("single solve (one slot):");
    for &(l, w) in &[(10usize, 30usize), (16, 64), (64, 128), (256, 256)] {
        let t = build(l, w, 0xABCD);
        let mut solver = Solver::new();
        let iters = (20_000_000 / (l * w * w) as u32).max(50);
        bench(&format!("L={l} W={w}"), iters, || solver.solve(&t).unwrap());
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
            secs * 1000.0
        );
    }
}

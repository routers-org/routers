//! A layered transition graph (trellis) and a single-threaded Viterbi solver.
//!
//! Edges exist only between adjacent layers. The solver finds the minimum-cost
//! path through all layers, treating layer 0 as a virtual start (every node
//! reachable at cost 0) and the last layer as a virtual end (best node wins).
//!
//! A single solve is intentionally single-threaded and allocation-free after
//! warm-up (see [`Solver`], a reusable "slot"). Throughput scales by running
//! many independent solves across slots; see [`solve_batch`].

use crate::{backend::Backend, trellis::INF_W};

mod backend;
mod path;
mod trellis;

pub use path::Path;
pub use trellis::Trellis;

/// A reusable solve "slot": owns scratch buffers so repeated solves don't
/// allocate, and caches the best SIMD backend the CPU supports. Create one per
/// worker thread.
pub struct Solver {
    dist: Vec<u32>,
    offsets: Vec<usize>,
    path: Vec<usize>,
    backend: Backend,
}

impl Default for Solver {
    fn default() -> Self {
        Self::new()
    }
}

impl Solver {
    pub fn new() -> Self {
        Solver {
            dist: Vec::new(),
            offsets: Vec::new(),
            path: Vec::new(),
            backend: Backend::default(),
        }
    }

    /// Minimum-cost path through `t`. Reuses internal buffers across calls.
    pub fn solve(&mut self, t: &Trellis) -> Path {
        let layers = t.layers();

        self.offsets.clear();
        self.offsets.push(0);

        let mut acc = 0usize;
        for &w in t.widths() {
            acc += w;
            self.offsets.push(acc);
        }

        if self.dist.len() < acc {
            self.dist.resize(acc, 0);
        }

        if self.path.len() < layers {
            self.path.resize(layers, 0);
        }

        // Virtual start: every layer-0 node reachable at cost 0.
        for x in &mut self.dist[0..t.widths()[0]] {
            *x = 0;
        }

        for tr in 0..layers - 1 {
            let nw = t.widths()[tr + 1];
            let split = self.offsets[tr + 1];

            let (head, tail) = self.dist.split_at_mut(split);

            let cur = &head[self.offsets[tr]..self.offsets[tr] + t.widths()[tr]];
            let next = &mut tail[..nw];

            self.backend
                .dispatch(cur, t.layer(tr), t.widths()[tr], nw, next);
        }

        self.backtrack(t)
    }

    fn backtrack(&mut self, t: &Trellis) -> Path {
        let l = t.layers();

        let last = l - 1;
        let lo = self.offsets[last];

        let mut bj = 0usize;
        let mut bv = INF_W;

        for j in 0..t.widths()[last] {
            let v = self.dist[lo + j];
            if v < bv {
                bv = v;
                bj = j;
            }
        }

        if bv >= INF_W {
            return Path {
                nodes: Vec::new(),
                cost: bv,
                reachable: false,
            };
        }

        self.path[last] = bj;
        for tr in (0..l - 1).rev() {
            let j = self.path[tr + 1];
            let nw = t.widths()[tr + 1];
            let prev = self.offsets[tr];
            let row = &t.layer(tr);

            let mut bi = 0usize;
            let mut best = INF_W;
            for i in 0..t.widths()[tr] {
                let v = self.dist[prev + i].saturating_add(row[i * nw + j]);
                if v < best {
                    best = v;
                    bi = i;
                }
            }

            self.path[tr] = bi;
        }

        Path {
            nodes: self.path[..l].to_vec(),
            cost: bv,
            reachable: true,
        }
    }
}

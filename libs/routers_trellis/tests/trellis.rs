use routers_trellis::*;

/// Width-1 chain: one edge per transition, so a single fully-specified path.
fn line(weights: &[u32]) -> Trellis {
    let mut t = Trellis::new(vec![1; weights.len() + 1]).unwrap();
    for (l, &w) in weights.iter().enumerate() {
        t.set_edge(l, 0, 0, w).unwrap();
    }
    t
}

/// Dense random trellis with uniform-width layers, seeded deterministically.
fn random_trellis(layers: usize, width: usize, seed: u64) -> Trellis {
    let mut t = Trellis::new(vec![width; layers]).unwrap();
    let mut s = seed | 1;
    let mut rng = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((s >> 40) as u32) % 100
    };
    for layer in 0..layers - 1 {
        let row: Vec<u32> = (0..width * width).map(|_| rng()).collect();
        t.fill_transition(layer, &row).unwrap();
    }
    t
}

// ---- Pending / Resolved state machine ----

#[test]
fn new_trellis_is_all_pending() {
    let t = Trellis::new(vec![2, 3, 2]).unwrap();
    assert!(!t.fully_resolved());
    assert_eq!(t.first_pending(), Some(0));
    assert!(!t.is_resolved(0) && !t.is_resolved(1));
}

#[test]
fn solve_on_pending_reports_first_unresolved_layer() {
    let mut t = Trellis::new(vec![2, 2, 2]).unwrap();
    assert_eq!(
        ViterbiSolver::new().solve(&t),
        Err(SolveError::NotResolved(0))
    );
    t.fill_transition(0, &[1, 1, 1, 1]).unwrap();
    assert!(t.is_resolved(0) && !t.is_resolved(1));
    assert_eq!(
        ViterbiSolver::new().solve(&t),
        Err(SolveError::NotResolved(1))
    );
}

#[test]
fn set_edge_resolves_a_pending_transition() {
    let mut t = Trellis::new(vec![2, 2]).unwrap();
    assert!(!t.is_resolved(0));
    t.set_edge(0, 0, 1, 7).unwrap();
    assert!(t.is_resolved(0) && t.fully_resolved());
}

#[test]
fn mark_pending_resets_state() {
    let mut t = Trellis::new(vec![2, 2]).unwrap();
    t.fill_transition(0, &[1, 2, 3, 4]).unwrap();
    assert!(t.fully_resolved());
    t.mark_pending(0).unwrap();
    assert_eq!(t.first_pending(), Some(0));
}

// ---- solving ----

#[test]
fn single_layer_is_zero_cost() {
    let p = ViterbiSolver::new()
        .solve(&Trellis::new(vec![3]).unwrap())
        .unwrap();
    assert!(p.reachable);
    assert_eq!(p.cost, 0);
    assert_eq!(p.nodes, vec![0]);
}

#[test]
fn straight_chain_sums_weights() {
    let p = ViterbiSolver::new().solve(&line(&[2, 3, 5])).unwrap();
    assert_eq!((p.cost, p.nodes), (10, vec![0, 0, 0, 0]));
}

#[test]
fn picks_cheaper_branch_and_respects_missing_edges() {
    let mut t = Trellis::new(vec![1, 2, 1]).unwrap();
    t.set_edge(0, 0, 0, 5).unwrap();
    t.set_edge(0, 0, 1, 1).unwrap();
    t.set_edge(1, 1, 0, 7).unwrap(); // node 0 of middle layer is a dead end
    let p = ViterbiSolver::new().solve(&t).unwrap();
    assert_eq!((p.cost, p.nodes), (8, vec![0, 1, 0]));
}

#[test]
fn resolved_but_disconnected_is_unreachable_not_pending() {
    let mut t = Trellis::new(vec![1, 1, 1]).unwrap();
    t.fill_transition(0, &[NO_EDGE]).unwrap();
    t.fill_transition(1, &[NO_EDGE]).unwrap();
    let p = ViterbiSolver::new().solve(&t).unwrap();
    assert!(!p.reachable);
}

#[test]
fn fill_transition_matches_set_edge() {
    let mut a = Trellis::new(vec![2, 2]).unwrap();
    a.fill_transition(0, &[1, NO_EDGE, NO_EDGE, 4]).unwrap();
    let mut b = Trellis::new(vec![2, 2]).unwrap();
    b.set_edge(0, 0, 0, 1).unwrap();
    b.set_edge(0, 1, 1, 4).unwrap();
    assert_eq!(
        ViterbiSolver::new().solve(&a),
        ViterbiSolver::new().solve(&b)
    );
}

#[test]
fn reused_solver_matches_fresh_solver() {
    let graphs = [line(&[1, 1]), line(&[2, 9, 1]), line(&[4])];
    let mut reused = ViterbiSolver::new();
    for g in &graphs {
        assert_eq!(reused.solve(g), ViterbiSolver::new().solve(g));
    }
}

#[test]
fn wide_dense_solve_is_consistent_across_runs() {
    let t = random_trellis(12, 40, 0xDEAD);
    let p1 = ViterbiSolver::new().solve(&t).unwrap();

    assert_eq!(p1, ViterbiSolver::new().solve(&t).unwrap());
    assert!(p1.reachable);
    assert_eq!(p1.nodes.len(), 12);
}

#[test]
fn rejects_oversized_weight() {
    let mut t = Trellis::new(vec![1, 1]).unwrap();
    assert!(matches!(
        t.set_edge(0, 0, 0, MAX_WEIGHT + 1),
        Err(TrellisError::WeightTooLarge(_))
    ));
    assert!(t.set_edge(0, 0, 0, MAX_WEIGHT).is_ok());
}

// ---- A/B conformance: Viterbi vs BruteForce ----

fn conformance(t: &Trellis) {
    let viterbi = ViterbiSolver::new().solve(t).unwrap();
    let brute = BruteForceSolver::new().solve(t).unwrap();
    assert_eq!(
        viterbi.cost, brute.cost,
        "cost mismatch: viterbi={} brute={}",
        viterbi.cost, brute.cost
    );
    assert_eq!(viterbi.reachable, brute.reachable, "reachability mismatch");
    // If reachable, verify both paths actually achieve the claimed cost.
    if viterbi.reachable {
        let viterbi_actual = path_cost(t, &viterbi.nodes);
        let brute_actual = path_cost(t, &brute.nodes);
        assert_eq!(viterbi_actual, viterbi.cost, "viterbi path cost incorrect");
        assert_eq!(brute_actual, brute.cost, "brute path cost incorrect");
    }
}

/// Compute the actual cost of `nodes` through `t` (for path validation).
fn path_cost(t: &Trellis, nodes: &[usize]) -> u32 {
    let mut cost = 0u32;
    for layer in 0..nodes.len() - 1 {
        let w = t.edge_weight(layer, nodes[layer], nodes[layer + 1]);
        cost = cost.saturating_add(w);
    }
    cost
}

#[test]
fn conformance_line_graph() {
    conformance(&line(&[1, 5, 2, 9, 3]));
}

#[test]
fn conformance_single_layer() {
    conformance(&Trellis::new(vec![4]).unwrap());
}

#[test]
fn conformance_two_layer_dense() {
    let mut t = Trellis::new(vec![3, 4]).unwrap();
    t.fill_transition(0, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
        .unwrap();
    conformance(&t);
}

#[test]
fn conformance_fully_disconnected() {
    let mut t = Trellis::new(vec![2, 2, 2]).unwrap();
    t.fill_transition(0, &[NO_EDGE; 4]).unwrap();
    t.fill_transition(1, &[NO_EDGE; 4]).unwrap();
    conformance(&t);
}

#[test]
fn conformance_partial_edges() {
    // Some edges absent, forcing both solvers to find the same narrow path.
    let mut t = Trellis::new(vec![3, 3, 3]).unwrap();
    t.set_edge(0, 0, 1, 10).unwrap();
    t.set_edge(0, 2, 2, 5).unwrap();
    t.set_edge(1, 1, 0, 3).unwrap();
    t.set_edge(1, 2, 2, 1).unwrap();
    conformance(&t);
}

#[test]
fn conformance_random_small() {
    for seed in 0u64..20 {
        conformance(&random_trellis(5, 6, seed));
    }
}

#[test]
fn conformance_random_varied_widths() {
    // Non-uniform widths exercise the row-major indexing differently.
    let widths = vec![2, 5, 3, 4, 2];
    let mut t = Trellis::new(widths.clone()).unwrap();
    let mut s: u64 = 0xCAFE_BABE;
    let mut rng = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((s >> 40) as u32) % 50
    };
    for layer in 0..widths.len() - 1 {
        let size = widths[layer] * widths[layer + 1];
        let row: Vec<u32> = (0..size).map(|_| rng()).collect();
        t.fill_transition(layer, &row).unwrap();
    }
    conformance(&t);
}

#[test]
fn brute_force_errors_on_pending() {
    let t = Trellis::new(vec![2, 2]).unwrap();
    assert_eq!(
        BruteForceSolver::new().solve(&t),
        Err(SolveError::NotResolved(0))
    );
}

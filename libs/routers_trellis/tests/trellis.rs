use routers_trellis::Trellis;

/// width-1 chain; one edge per transition, so every transition is resolved.
fn line(weights: &[u32]) -> Trellis {
    let mut t = Trellis::new(vec![1; weights.len() + 1]).unwrap();
    for (l, &w) in weights.iter().enumerate() {
        t.set_edge(l, 0, 0, w).unwrap();
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
    assert_eq!(Solver::new().solve(&t), Err(SolveError::NotResolved(0)));
    t.fill_transition(0, &[1, 1, 1, 1]).unwrap(); // resolve layer 0 only
    assert!(t.is_resolved(0) && !t.is_resolved(1));
    assert_eq!(Solver::new().solve(&t), Err(SolveError::NotResolved(1)));
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
    let p = Solver::new()
        .solve(&Trellis::new(vec![3]).unwrap())
        .unwrap();
    assert!(p.reachable);
    assert_eq!(p.cost, 0);
    assert_eq!(p.nodes, vec![0]);
}

#[test]
fn straight_chain_sums_weights() {
    let p = Solver::new().solve(&line(&[2, 3, 5])).unwrap();
    assert_eq!((p.cost, p.nodes), (10, vec![0, 0, 0, 0]));
}

#[test]
fn picks_cheaper_branch_and_respects_missing_edges() {
    let mut t = Trellis::new(vec![1, 2, 1]).unwrap();
    t.set_edge(0, 0, 0, 5).unwrap();
    t.set_edge(0, 0, 1, 1).unwrap();
    t.set_edge(1, 1, 0, 7).unwrap(); // node 0 of middle layer is a dead end
    let p = Solver::new().solve(&t).unwrap();
    assert_eq!((p.cost, p.nodes), (8, vec![0, 1, 0]));
}

#[test]
fn resolved_but_disconnected_is_unreachable_not_pending() {
    // Both transitions resolved but every edge absent: solvable, no path.
    let mut t = Trellis::new(vec![1, 1, 1]).unwrap();
    t.fill_transition(0, &[NO_EDGE]).unwrap();
    t.fill_transition(1, &[NO_EDGE]).unwrap();
    let p = Solver::new().solve(&t).unwrap();
    assert!(!p.reachable);
}

#[test]
fn fill_transition_matches_set_edge() {
    let mut a = Trellis::new(vec![2, 2]).unwrap();
    a.fill_transition(0, &[1, NO_EDGE, NO_EDGE, 4]).unwrap();
    let mut b = Trellis::new(vec![2, 2]).unwrap();
    b.set_edge(0, 0, 0, 1).unwrap();
    b.set_edge(0, 1, 1, 4).unwrap();
    assert_eq!(Solver::new().solve(&a), Solver::new().solve(&b));
}

#[test]
fn reused_solver_matches_fresh_solver() {
    let graphs = [line(&[1, 1]), line(&[2, 9, 1]), line(&[4])];
    let mut reused = Solver::new();
    for g in &graphs {
        assert_eq!(reused.solve(g), Solver::new().solve(g));
    }
}

#[test]
fn wide_dense_solve_is_consistent_across_runs() {
    let (l, w) = (12usize, 40usize);
    let mut t = Trellis::new(vec![w; l]).unwrap();
    let mut s: u64 = 1;
    let mut rng = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((s >> 40) as u32) % 100
    };
    for layer in 0..l - 1 {
        let row: Vec<u32> = (0..w * w).map(|_| rng()).collect();
        t.fill_transition(layer, &row).unwrap();
    }
    let p1 = Solver::new().solve(&t).unwrap();
    assert_eq!(p1, Solver::new().solve(&t).unwrap());
    assert!(p1.reachable);
    assert_eq!(p1.nodes.len(), l);
}

#[test]
fn batch_matches_sequential_and_propagates_pending() {
    let mut graphs: Vec<Trellis> = (1..=20).map(|k| line(&vec![k as u32; 5])).collect();
    graphs.push(Trellis::new(vec![2, 2]).unwrap()); // one left Pending
    let batched = solve_batch(&graphs, 4);
    let mut solver = Solver::new();
    for (g, b) in graphs.iter().zip(&batched) {
        assert_eq!(&solver.solve(g), b);
    }
    assert_eq!(batched.last().unwrap(), &Err(SolveError::NotResolved(0)));
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

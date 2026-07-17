use routers_trellis::*;

/// Width-1 chain: one edge per transition, so a single fully-specified path.
fn line(weights: &[u32]) -> Trellis {
    let mut t = Trellis::new(vec![1u32; weights.len() + 1]).unwrap();
    for (l, &w) in weights.iter().enumerate() {
        t.set_edge(LayerId(l as u32), NodeId(0), NodeId(0), w)
            .unwrap();
    }
    t
}

/// Dense random trellis with uniform-width layers, seeded deterministically.
/// Node weights stay zero — see [`random_noded_trellis`].
fn random_trellis(layers: usize, width: u32, seed: u64) -> Trellis {
    random_trellis_with(layers, width, seed, false)
}

/// [`random_trellis`], with the node weights randomised too.
fn random_noded_trellis(layers: usize, width: u32, seed: u64) -> Trellis {
    random_trellis_with(layers, width, seed, true)
}

fn random_trellis_with(layers: usize, width: u32, seed: u64, noded: bool) -> Trellis {
    let mut t = Trellis::new(vec![width; layers]).unwrap();
    let mut s = seed | 1;
    let mut rng = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((s >> 40) as u32) % 100
    };
    for layer in 0..layers - 1 {
        let row: Vec<u32> = (0..(width * width) as usize).map(|_| rng()).collect();
        t.fill_transition(LayerId(layer as u32), &row).unwrap();
    }
    if noded {
        for layer in 0..layers {
            let weights: Vec<u32> = (0..width).map(|_| rng()).collect();
            t.fill_nodes(LayerId(layer as u32), &weights).unwrap();
        }
    }
    t
}

// ---- Pending / Resolved state machine ----

#[test]
fn new_trellis_is_all_pending() {
    let t = Trellis::new(vec![2u32, 3, 2]).unwrap();
    assert!(!t.fully_resolved());
    assert_eq!(t.first_pending(), Some(LayerId(0)));
    assert!(!t.is_resolved(LayerId(0)) && !t.is_resolved(LayerId(1)));
}

#[test]
fn solve_on_pending_reports_first_unresolved_layer() {
    let mut t = Trellis::new(vec![2u32, 2, 2]).unwrap();
    assert_eq!(
        ViterbiSolver::new().solve(&t),
        Err(SolveError::NotResolved(LayerId(0)))
    );
    t.fill_transition(LayerId(0), &[1, 1, 1, 1]).unwrap();
    assert!(t.is_resolved(LayerId(0)) && !t.is_resolved(LayerId(1)));
    assert_eq!(
        ViterbiSolver::new().solve(&t),
        Err(SolveError::NotResolved(LayerId(1)))
    );
}

#[test]
fn set_edge_resolves_a_pending_transition() {
    let mut t = Trellis::new(vec![2u32, 2]).unwrap();
    assert!(!t.is_resolved(LayerId(0)));
    t.set_edge(LayerId(0), NodeId(0), NodeId(1), 7).unwrap();
    assert!(t.is_resolved(LayerId(0)) && t.fully_resolved());
}

#[test]
fn mark_pending_resets_state() {
    let mut t = Trellis::new(vec![2u32, 2]).unwrap();
    t.fill_transition(LayerId(0), &[1, 2, 3, 4]).unwrap();
    assert!(t.fully_resolved());
    t.mark_pending(LayerId(0)).unwrap();
    assert_eq!(t.first_pending(), Some(LayerId(0)));
}

// ---- solving ----

#[test]
fn single_layer_is_zero_cost() {
    let p = ViterbiSolver::new()
        .solve(&Trellis::new(vec![3u32]).unwrap())
        .unwrap();
    assert_eq!(p.cost, 0);
    assert_eq!(p.nodes, vec![NodeId(0)]);
}

#[test]
fn straight_chain_sums_weights() {
    let p = ViterbiSolver::new().solve(&line(&[2, 3, 5])).unwrap();
    assert_eq!(p.cost, 10);
    assert_eq!(p.nodes, vec![NodeId(0); 4]);
}

#[test]
fn picks_cheaper_branch_and_respects_missing_edges() {
    let mut t = Trellis::new(vec![1u32, 2, 1]).unwrap();
    t.set_edge(LayerId(0), NodeId(0), NodeId(0), 5).unwrap();
    t.set_edge(LayerId(0), NodeId(0), NodeId(1), 1).unwrap();
    t.set_edge(LayerId(1), NodeId(1), NodeId(0), 7).unwrap(); // node 0 of middle layer is a dead end
    let p = ViterbiSolver::new().solve(&t).unwrap();
    assert_eq!(p.cost, 8);
    assert_eq!(p.nodes, vec![NodeId(0), NodeId(1), NodeId(0)]);
}

#[test]
fn resolved_but_disconnected_is_unreachable_not_pending() {
    let mut t = Trellis::new(vec![1u32, 1, 1]).unwrap();
    t.fill_transition(LayerId(0), &[NO_EDGE]).unwrap();
    t.fill_transition(LayerId(1), &[NO_EDGE]).unwrap();
    assert_eq!(ViterbiSolver::new().solve(&t), Err(SolveError::Unreachable));
}

#[test]
fn fill_transition_matches_set_edge() {
    let mut a = Trellis::new(vec![2u32, 2]).unwrap();
    a.fill_transition(LayerId(0), &[1, NO_EDGE, NO_EDGE, 4])
        .unwrap();
    let mut b = Trellis::new(vec![2u32, 2]).unwrap();
    b.set_edge(LayerId(0), NodeId(0), NodeId(0), 1).unwrap();
    b.set_edge(LayerId(0), NodeId(1), NodeId(1), 4).unwrap();
    assert_eq!(
        ViterbiSolver::new().solve(&a),
        ViterbiSolver::new().solve(&b)
    );
}

#[test]
fn reused_solver_matches_fresh_solver() {
    let graphs = [line(&[1, 1]), line(&[2, 9, 1]), line(&[4])];
    let reused = ViterbiSolver::new();
    for g in &graphs {
        assert_eq!(reused.solve(g), ViterbiSolver::new().solve(g));
    }
}

#[test]
fn wide_dense_solve_is_consistent_across_runs() {
    let t = random_noded_trellis(12, 40, 0xDEAD);
    let p1 = ViterbiSolver::new().solve(&t).unwrap();
    assert_eq!(p1, ViterbiSolver::new().solve(&t).unwrap());
    assert_eq!(p1.nodes.len(), 12);
}

#[test]
fn rejects_oversized_weight() {
    let mut t = Trellis::new(vec![1u32, 1]).unwrap();
    assert!(matches!(
        t.set_edge(LayerId(0), NodeId(0), NodeId(0), MAX_WEIGHT + 1),
        Err(TrellisError::WeightTooLarge(_))
    ));
    assert!(
        t.set_edge(LayerId(0), NodeId(0), NodeId(0), MAX_WEIGHT)
            .is_ok()
    );
}

// ---- node weights ----

#[test]
fn node_weights_default_to_zero() {
    let t = Trellis::new(vec![2u32, 3]).unwrap();
    assert_eq!(t.node_weights(LayerId(1)), Some([0u32, 0, 0].as_slice()));
    assert_eq!(t.node_weight(LayerId(0), NodeId(1)), Some(0));
    assert_eq!(t.node_weight(LayerId(0), NodeId(2)), None);
}

#[test]
fn fill_nodes_validates_length_and_ceiling() {
    let mut t = Trellis::new(vec![2u32, 2]).unwrap();
    assert!(matches!(
        t.fill_nodes(LayerId(0), &[1]),
        Err(TrellisError::NodeLenMismatch { .. })
    ));
    assert!(matches!(
        t.fill_nodes(LayerId(0), &[1, MAX_WEIGHT + 1]),
        Err(TrellisError::WeightTooLarge(_))
    ));
    assert!(matches!(
        t.fill_nodes(LayerId(2), &[1, 1]),
        Err(TrellisError::LayerOutOfRange(_))
    ));
    t.fill_nodes(LayerId(1), &[3, 4]).unwrap();
    assert_eq!(t.node_weight(LayerId(1), NodeId(1)), Some(4));
}

#[test]
fn first_layer_node_weight_steers_the_start() {
    // Two equal-cost chains; only the first layer's node weights differ.
    let mut t = Trellis::new(vec![2u32, 1]).unwrap();
    t.fill_transition(LayerId(0), &[5, 5]).unwrap();
    t.fill_nodes(LayerId(0), &[9, 2]).unwrap();
    let p = ViterbiSolver::new().solve(&t).unwrap();
    assert_eq!(p.nodes[0], NodeId(1));
    assert_eq!(p.cost, 7);
}

#[test]
fn node_weights_reroute_an_otherwise_cheaper_path() {
    // Edge costs prefer node 0 of the middle layer; its node weight repels.
    let mut t = Trellis::new(vec![1u32, 2, 1]).unwrap();
    t.fill_transition(LayerId(0), &[1, 3]).unwrap();
    t.fill_transition(LayerId(1), &[1, 3]).unwrap();
    t.fill_nodes(LayerId(1), &[10, 0]).unwrap();
    let p = ViterbiSolver::new().solve(&t).unwrap();
    assert_eq!(p.nodes, vec![NodeId(0), NodeId(1), NodeId(0)]);
    assert_eq!(p.cost, 6);
}

#[test]
fn single_layer_picks_lightest_node() {
    let mut t = Trellis::new(vec![3u32]).unwrap();
    t.fill_nodes(LayerId(0), &[4, 1, 2]).unwrap();
    let p = ViterbiSolver::new().solve(&t).unwrap();
    assert_eq!(p.nodes, vec![NodeId(1)]);
    assert_eq!(p.cost, 1);
}

// ---- structure ----

#[test]
fn add_layer_returns_sequential_ids() {
    let mut t = Trellis::new(vec![2u32]).unwrap();
    assert_eq!(t.last_id(), LayerId(0));
    assert_eq!(t.add_layer(3), Ok(LayerId(1)));
    assert_eq!(t.add_layer(1), Ok(LayerId(2)));
    assert_eq!(t.last_id(), LayerId(2));
    assert_eq!(t.node_weights(LayerId(2)), Some([0u32].as_slice()));
}

#[test]
fn add_layer_rejects_zero_width() {
    let mut t = Trellis::new(vec![2u32]).unwrap();
    assert_eq!(
        t.add_layer(0),
        Err(TrellisError::ZeroWidthLayer(LayerId(1)))
    );
}

// ---- windowing ----

#[test]
fn partition_keeps_interior_weights_and_nodes() {
    let t = random_noded_trellis(6, 3, 0xF00D);
    let w = t.partition(LayerId(2)..LayerId(5)).unwrap();

    assert_eq!(w.layers(), 3);
    assert_eq!(w.node_weights(LayerId(0)), t.node_weights(LayerId(2)));
    for (from, to) in [(0u32, 2u32), (1, 3)] {
        for a in 0..3u32 {
            for b in 0..3u32 {
                assert_eq!(
                    w.edge_weight(LayerId(from), NodeId(a), NodeId(b)),
                    t.edge_weight(LayerId(to), NodeId(a), NodeId(b)),
                );
            }
        }
    }
}

#[test]
fn last_windows_the_tail() {
    let t = random_noded_trellis(6, 3, 0xBEEF);
    let w = t.last(2).unwrap();
    assert_eq!(w.layers(), 2);
    assert_eq!(w.node_weights(LayerId(1)), t.node_weights(LayerId(5)));

    // Oversized n clamps to the whole trellis; zero is empty.
    assert_eq!(t.last(100).unwrap().layers(), 6);
    assert_eq!(t.last(0), Err(TrellisError::Empty));
}

#[test]
fn partition_rejects_bad_ranges() {
    let t = random_trellis(4, 2, 1);
    assert_eq!(
        t.partition(LayerId(2)..LayerId(2)),
        Err(TrellisError::Empty)
    );
    assert_eq!(
        t.partition(LayerId(1)..LayerId(9)),
        Err(TrellisError::LayerOutOfRange(LayerId(9)))
    );
}

#[test]
fn windowed_solve_matches_direct_construction() {
    let t = random_noded_trellis(8, 4, 0x5EED);
    let w = t.last(3).unwrap();
    let direct = ViterbiSolver::new().solve(&w).unwrap();
    let brute = BruteForceSolver::new().solve(&w).unwrap();
    assert_eq!(direct.cost, brute.cost);
}

// ---- Solved certificate ----

#[test]
fn solve_certifies_and_append_reopens() {
    let t = random_noded_trellis(4, 3, 0xACE);
    let solved = t.solve(&ViterbiSolver::new()).unwrap();
    assert_eq!(solved.path().nodes.len(), 4);
    assert_eq!(solved.cost(), solved.path().cost);

    let (mut t, id) = solved.append(2).unwrap();
    assert_eq!(id, LayerId(4));
    assert_eq!(t.first_pending(), Some(LayerId(3)));

    // Weigh the new boundary and it solves again.
    t.fill_transition(LayerId(3), &[1, 1, 1, 1, 1, 1]).unwrap();
    let solved = t.solve(&ViterbiSolver::new()).unwrap();
    assert_eq!(solved.path().nodes.len(), 5);
}

#[test]
fn failed_solve_hands_the_trellis_back() {
    let t = Trellis::new(vec![2u32, 2]).unwrap();
    let (t, e) = t.solve(&ViterbiSolver::new()).unwrap_err();
    assert_eq!(e, SolveError::NotResolved(LayerId(0)));
    assert_eq!(t.layers(), 2); // intact, still usable
}

#[test]
fn rejected_append_hands_the_certificate_back() {
    let t = line(&[1]);
    let solved = t.solve(&ViterbiSolver::new()).unwrap();
    let (solved, e) = solved.append(0).unwrap_err();
    assert_eq!(e, TrellisError::ZeroWidthLayer(LayerId(2)));
    assert_eq!(solved.path().nodes.len(), 2); // certificate intact
}

#[test]
fn reopen_permits_surgery_then_resolve() {
    let solved = line(&[1, 2]).solve(&ViterbiSolver::new()).unwrap();
    let mut t = solved.reopen();
    t.mark_pending(LayerId(0)).unwrap();
    assert_eq!(
        ViterbiSolver::new().solve(&t),
        Err(SolveError::NotResolved(LayerId(0)))
    );
    t.set_edge(LayerId(0), NodeId(0), NodeId(0), 9).unwrap();
    assert_eq!(t.solve(&ViterbiSolver::new()).unwrap().cost(), 11);
}

#[cfg(feature = "serde")]
#[test]
fn solved_round_trips_through_serde() {
    let solved = random_noded_trellis(4, 3, 0xD1CE)
        .solve(&ViterbiSolver::new())
        .unwrap();
    let json = serde_json::to_string(&solved).unwrap();
    let back: Solved = serde_json::from_str(&json).unwrap();
    assert_eq!(back.path(), solved.path());
    assert_eq!(back.trellis().widths(), solved.trellis().widths());
}

// ---- A/B conformance: Viterbi vs BruteForce ----

fn conformance(t: &Trellis) {
    let viterbi = ViterbiSolver::new().solve(t);
    let brute = BruteForceSolver::new().solve(t);

    match (&viterbi, &brute) {
        (Ok(v), Ok(b)) => {
            assert_eq!(v.cost, b.cost, "cost mismatch: viterbi={v:?} brute={b:?}");
            assert_eq!(t.path_cost(&v.nodes), v.cost, "viterbi path cost incorrect");
            assert_eq!(t.path_cost(&b.nodes), b.cost, "brute path cost incorrect");
        }
        (Err(v), Err(b)) => assert_eq!(v, b, "error mismatch"),
        _ => panic!("solver disagreement: viterbi={viterbi:?} brute={brute:?}"),
    }
}

#[test]
fn conformance_line_graph() {
    conformance(&line(&[1, 5, 2, 9, 3]));
}

#[test]
fn conformance_single_layer() {
    conformance(&Trellis::new(vec![4u32]).unwrap());
}

#[test]
fn conformance_two_layer_dense() {
    let mut t = Trellis::new(vec![3u32, 4]).unwrap();
    t.fill_transition(LayerId(0), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
        .unwrap();
    conformance(&t);
}

#[test]
fn conformance_fully_disconnected() {
    let mut t = Trellis::new(vec![2u32, 2, 2]).unwrap();
    t.fill_transition(LayerId(0), &[NO_EDGE; 4]).unwrap();
    t.fill_transition(LayerId(1), &[NO_EDGE; 4]).unwrap();
    conformance(&t);
}

#[test]
fn conformance_partial_edges() {
    let mut t = Trellis::new(vec![3u32, 3, 3]).unwrap();
    t.set_edge(LayerId(0), NodeId(0), NodeId(1), 10).unwrap();
    t.set_edge(LayerId(0), NodeId(2), NodeId(2), 5).unwrap();
    t.set_edge(LayerId(1), NodeId(1), NodeId(0), 3).unwrap();
    t.set_edge(LayerId(1), NodeId(2), NodeId(2), 1).unwrap();
    conformance(&t);
}

#[test]
fn conformance_random_small() {
    for seed in 0u64..20 {
        conformance(&random_trellis(5, 6, seed));
    }
}

#[test]
fn conformance_random_small_with_node_weights() {
    for seed in 0u64..20 {
        conformance(&random_noded_trellis(5, 6, seed));
    }
}

#[test]
fn conformance_random_varied_widths() {
    let widths = vec![2u32, 5, 3, 4, 2];
    let mut t = Trellis::new(widths.clone()).unwrap();
    let mut s: u64 = 0xCAFE_BABE;
    let mut rng = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((s >> 40) as u32) % 50
    };
    for layer in 0..widths.len() - 1 {
        let size = (widths[layer] * widths[layer + 1]) as usize;
        let row: Vec<u32> = (0..size).map(|_| rng()).collect();
        t.fill_transition(LayerId(layer as u32), &row).unwrap();
    }
    for (layer, &width) in widths.iter().enumerate() {
        let weights: Vec<u32> = (0..width).map(|_| rng()).collect();
        t.fill_nodes(LayerId(layer as u32), &weights).unwrap();
    }
    conformance(&t);
}

#[test]
fn brute_force_errors_on_pending() {
    let t = Trellis::new(vec![2u32, 2]).unwrap();
    assert_eq!(
        BruteForceSolver::new().solve(&t),
        Err(SolveError::NotResolved(LayerId(0)))
    );
}

/// The same solver interleaved across two unrelated trellises stays correct:
/// the solver holds only scratch, no per-trellis state.
#[test]
fn one_solver_interleaved_across_two_trellises() {
    let a = random_noded_trellis(6, 5, 0xAAAA);
    let b = random_noded_trellis(9, 3, 0xBBBB);

    let solver = ViterbiSolver::new();
    for _ in 0..3 {
        assert_eq!(solver.solve(&a), ViterbiSolver::new().solve(&a));
        assert_eq!(solver.solve(&b), ViterbiSolver::new().solve(&b));
    }
}

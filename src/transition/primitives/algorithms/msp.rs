use std::collections::HashSet;
use pathfinding::num_traits::Zero;

use crate::AStar;
use crate::astar::AStarReachableItem;
use crate::transition::WeightAndDistance;

pub struct MultiTargetSearch<E>
where
    E: routers_codec::Entry,
{
    targets: HashSet<E>,
    found_targets: HashSet<E>,
    max_reasonable_distance: WeightAndDistance,
    last_found_distance: WeightAndDistance,
    nodes_since_last_find: usize,
    max_nodes_without_progress: usize,
}

impl<E> MultiTargetSearch<E>
where
    E: routers_codec::Entry,
{
    pub fn new(
        targets: HashSet<E>,
        max_reasonable_distance: WeightAndDistance,
        max_nodes_without_progress: usize,
    ) -> Self {
        Self {
            targets,
            found_targets: HashSet::new(),
            max_reasonable_distance,
            last_found_distance: WeightAndDistance::zero(),
            nodes_since_last_find: 0,
            max_nodes_without_progress,
        }
    }

    /// Returns true if we should stop the search
    pub fn should_stop(&mut self, item: &AStarReachableItem<E>) -> bool {
        // Check if we found a target
        if self.targets.contains(&item.node) {
            self.found_targets.insert(item.node);
            self.last_found_distance = item.total_cost;
            self.nodes_since_last_find = 0;

            // Stop if we've found all targets
            if self.found_targets.len() == self.targets.len() {
                return true;
            }
        } else {
            self.nodes_since_last_find += 1;
        }

        // Stop if we're exploring too far beyond reasonable distance
        if item.total_cost > self.max_reasonable_distance {
            return true;
        }

        // Stop if we haven't found a target in too many nodes
        // (indicates remaining targets might be inaccessible)
        if self.nodes_since_last_find > self.max_nodes_without_progress {
            return true;
        }

        false
    }

    pub fn get_found_targets(&self) -> &HashSet<E> {
        &self.found_targets
    }

    pub fn get_unfound_targets(&self) -> HashSet<E> {
        self.targets.difference(&self.found_targets).cloned().collect()
    }

    pub fn found_all_targets(&self) -> bool {
        self.found_targets.len() == self.targets.len()
    }
}

// Usage example:
pub fn find_multiple_targets<E, FN, HN, IN>(
    start: &E,
    targets: HashSet<E>,
    successors: FN,
    heuristic: HN,
    max_distance: WeightAndDistance,
) -> (HashSet<E>, HashSet<E>) // (found, unfound)
where
    E: routers_codec::Entry,
    FN: FnMut(&E) -> IN,
    HN: FnMut(&E) -> WeightAndDistance,
    IN: Iterator<Item = (E, WeightAndDistance)>,
{
    let mut search = MultiTargetSearch::new(
        targets.clone(),
        max_distance,
        1000, // max nodes without progress
    );

    let astar = AStar;
    let reachable = astar.reach(start, successors, heuristic);

    for item in reachable {
        if search.should_stop(&item) {
            break;
        }

        // You can still process other nodes here if needed
        // e.g., store reachable nodes, update UI, etc.
    }

    (search.get_found_targets().clone(), search.get_unfound_targets())
}

// Alternative: More sophisticated heuristic-based stopping
pub fn find_targets_with_heuristic_cutoff<E, FN, HN, IN>(
    start: &E,
    targets: HashSet<E>,
    successors: FN,
    mut heuristic: HN,
    max_distance: WeightAndDistance,
) -> (HashSet<E>, HashSet<E>)
where
    E: routers_codec::Entry,
    FN: FnMut(&E) -> IN,
    HN: FnMut(&E) -> WeightAndDistance,
    IN: Iterator<Item = (E, WeightAndDistance)>,
{
    let mut found_targets = HashSet::new();
    let mut last_promising_distance = WeightAndDistance::zero();

    let reachable = AStar.reach(start, successors, heuristic);

    for item in reachable {
        // Found a target
        if targets.contains(&item.node) {
            found_targets.insert(item.node);
            last_promising_distance = item.total_cost;

            // Stop if we've found all targets
            if found_targets.len() == targets.len() {
                break;
            }
        }

        // Estimate total cost to reach this node + best case to any remaining target
        let current_g = item.total_cost;
        let min_h_to_remaining = targets.iter()
            .filter(|t| !found_targets.contains(t))
            .map(|t| heuristic(t))
            .min()
            .unwrap_or(WeightAndDistance::zero());

        let estimated_total = current_g + min_h_to_remaining;

        // Stop if even the most optimistic estimate exceeds our limit
        if estimated_total > max_distance {
            break;
        }
    }

    let unfound_targets = targets.difference(&found_targets).cloned().collect();
    (found_targets, unfound_targets)
}
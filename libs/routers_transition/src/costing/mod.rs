//! Costing: the heuristics a match is priced by.
//!
//! Two costs shape every match. The **emission** cost prices anchoring an
//! input point to a candidate (how far-fetched is this road position for this
//! point?), and the **transition** cost prices travelling between two
//! candidates in adjacent layers (how plausible is the road route between
//! them?). Both are supplied together as a [`CostingStrategies`] pair and
//! handed to the [`Matcher`](crate::Matcher):
//!
//! ```ignore
//! use routers_transition::{CostingStrategies, Matcher};
//!
//! // The default heuristics; see below to bring your own.
//! let costing = CostingStrategies::default();
//!
//! let matcher = Matcher::new(&map, &costing, generator, weigher, &runtime);
//! ```
//!
//! The defaults — [`DefaultEmissionCost`] and [`DefaultTransitionCost`] —
//! suit road-vehicle GPS traces; their documentation details the exact
//! calculations and tunable hyperparameters.
//!
//! ## Bringing your own heuristic
//!
//! To replace either cost, implement [`Strategy`] for your type over the
//! matching context, and supply it through [`CostingStrategies::new`]. A
//! strategy returns a value in `[0, 1]` — `1` a perfect (free) choice, `0`
//! the most expensive — which the [`Strategy::cost`] decay function converts
//! into an integer weight.
//!
//! ```rust
//! use routers_network::Entry;
//! use routers_transition::{Strategy, TransitionContext};
//!
//! struct MyTransitionStrategy;
//!
//! // Implement the strategy with the correct context.
//! impl<'a, E> Strategy<TransitionContext<'a, E>> for MyTransitionStrategy where E: Entry {
//!    type Cost = f64;
//!
//!    const ZETA: f64 = 1.0;
//!    const BETA: f64 = -50.0;
//!
//!    fn calculate(&self, context: TransitionContext<'a, E>) -> Option<Self::Cost> {
//!        todo!()
//!    }
//! }
//! ```
//!
//! The context tells you which cost you are implementing, and carries
//! everything there is to know at that point:
//!
//! - [`EmissionContext`] — the input point, the candidate position, and the
//!   distance between them.
//! - [`TransitionContext`] — the two candidates, the optimal road path
//!   between them, and its geometry.
//!
//! The higher-order traits ([`EmissionStrategy`], [`TransitionStrategy`]) are
//! blanket-implemented for anything implementing [`Strategy`] over the right
//! context — there is nothing further to derive.

mod default;
mod emission;
mod transition;
mod util;

pub use default::{CostingStrategies, DefaultEmissionCost, DefaultTransitionCost};
pub use emission::{EmissionContext, EmissionStrategy};
pub use transition::{
    Headings, TransitionContext, TransitionLengths, TransitionStrategy, VirtualTails,
};
pub use util::{Costing, Strategy};

//! You may override individual costing strategies
//! in order to apply custom functionality to the
//! transition solver. See the [`Strategy`] trait.
//!
//! ## Structure
//! Strategies are joined onto the aggregate [`CostingStrategies`]
//! structure, which is then supplied to the relevant transition
//! graph constructor.
//!
//! ```rust,no_run
//! use std::ffi::OsString;
//! use routers::impls::osm::OsmGraph;
//! use routers::transition::{CostingStrategies, Transition};
//!
//! // Create a graph with a specific dataset, such as the OpenStreetMap impl.
//! let graph = OsmGraph::new(OsString::from("<path_to_file>.pbf"))
//!     .expect("Fatal: Must ingest");
//!
//! // Create default strategies
//! let costing = CostingStrategies::default();
//!
//! // Supply them to the relevant constructor
//! let transition = Transition::new(&graph, todo!(), costing);
//!```
//!
//! To override the default strategies, simply apply your own
//! using [`CostingStrategies::new`]. You must create an [`EmissionStrategy`]
//! and  [`TransitionStrategy`].
//!
//! ### Creating your own strategy / heuristic
//!
//! In order to make your own transition and emission strategies, you must
//! implement [`Strategy`] for your structure, with the context of the heuristic
//! you need to override.
//!
//! The higher-order traits, like [`TransitionStrategy`] are auto-derived for all
//! which implement [`Strategy<TransitionContext>`].
//!
//!```rust
//! use routers_codec::{Entry, Metadata};
//! use routers::transition::{Strategy, TransitionContext};
//!
//! struct MyTransitionStrategy;
//!
//! // Implement the strategy with the correct context.
//! impl<'a, E: Entry, M: Metadata> Strategy<TransitionContext<'a, E, M>> for MyTransitionStrategy {
//!    type Cost = f64;
//!
//!    const ZETA: f64 = 1.0;
//!    const BETA: f64 = -50.0;
//!
//!    fn calculate(&self, context: TransitionContext<'a, E, M>) -> Option<Self::Cost> {
//!        todo!()
//!    }
//! }
//! ```
//!
//! Note that each require consuming their own context. See below.
//!
//! ### Using Context
//! Each strategy accepts a context, defined in the
//! generic `Ctx` parameter of the [`Strategy`] trait.
//! Each context is defined statically:
//!
//! - [`TransitionContext`]
//!   Used for the transition costing strategy,
//!   supplies relevant information for the candidates
//!   being routed between, and the optimal trip between them.
//!
//! - [`EmissionContext`]
//!   Used to understand the cost associated with
//!   the selection of a candidate, relative to an optimal
//!   selection on the underlying routing data.
//!
//! There are two static heuristics which must have
//! a strategy defined for them in order to evaluate
//! the costing behind them. A default strategy for each
//! one is defined below.
//!
//! ### Default Strategies:
//! - [`DefaultTransitionCost`]: Transition Cost
//! - [`DefaultEmissionCost`]: Emission Cost
//!
#[doc(hidden)]
pub mod default;
#[doc(hidden)]
pub mod emission;
#[doc(hidden)]
pub mod routing;
#[doc(hidden)]
pub mod transition;
#[doc(hidden)]
pub mod util;

#[doc(inline)]
pub use default::*;
#[doc(inline)]
pub use emission::*;
#[doc(inline)]
pub use routing::*;
#[doc(inline)]
pub use transition::*;
#[doc(inline)]
pub use util::*;

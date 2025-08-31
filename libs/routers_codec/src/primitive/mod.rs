use std::fmt::Debug;
use std::hash::Hash;

pub mod edge;
pub mod node;
pub mod transport;

pub use edge::Direction;
pub use edge::Edge;
pub use node::Node;

pub mod context {
    use crate::primitive::transport::TransportMode;

    pub struct TripContext {
        pub transport_mode: TransportMode,
    }
}

pub trait Entry:
    Default + Copy + Clone + PartialEq + Eq + Ord + Hash + Debug + Send + Sync
{
    fn identifier(&self) -> i64;
}

pub trait Metadata: Clone + Debug + Send + Sync {
    type Raw<'a>
    where
        Self: 'a;

    type Runtime: Debug + Send + Sync;
    type TripContext;

    fn pick(raw: Self::Raw<'_>) -> Self;
    fn runtime(ctx: Option<Self::TripContext>) -> Self::Runtime;
    fn accessible(&self, access: &Self::Runtime, direction: Direction) -> bool;

    /// The default runtime for the specific metadata implementation
    fn default_runtime() -> Self::Runtime {
        Self::runtime(None)
    }
}

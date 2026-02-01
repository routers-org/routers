use crate::Direction;
use core::fmt::Debug;
use serde::Serialize;

/// TODO: Description
pub trait Metadata: Clone + Debug + Serialize + Send + Sync {
    /// TODO: Describe
    type Raw<'a>
    where
        Self: 'a;

    /// TODO: Describe
    type Runtime: Clone + Debug + Send + Sync;

    /// TODO: Describe
    type TripContext;

    /// TODO: Describe
    fn pick(raw: Self::Raw<'_>) -> Self;

    /// TODO: Describe
    fn runtime(ctx: Option<Self::TripContext>) -> Self::Runtime;

    /// TODO: Describe
    fn accessible(&self, access: &Self::Runtime, direction: Direction) -> bool;

    /// The default runtime for the specific metadata implementation
    fn default_runtime() -> Self::Runtime {
        Self::runtime(None)
    }
}

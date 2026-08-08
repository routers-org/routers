use crate::Direction;
use core::fmt::Debug;
use serde::Serialize;

/// TODO: Description
pub trait Metadata: Clone + Debug + Serialize + Send + Sync {
    /// TODO: Describe
    type Raw<'a>
    where
        Self: 'a;

    /// The runtime routing configuration (mode, vehicle properties, time,
    /// private-access) that [`accessible`](Self::accessible) is evaluated
    /// against.
    ///
    /// `PartialEq` is required so accessibility-dependent caches can assert
    /// they are only ever served one runtime — an edge's accessibility is a
    /// function of the runtime, so a cache keyed without it would taint results
    /// across configurations. (It is `PartialEq` rather than `Eq`/`Hash`
    /// because a runtime may carry `f32` vehicle properties.)
    type Runtime: Clone + Debug + Send + Sync + PartialEq;

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

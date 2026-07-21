use core::fmt::Debug;
use core::hash::Hash;
use serde::Serialize;

/// TODO: Description
///
/// `'static` because entry identifiers own no borrows (they are `Copy` node
/// keys) and are held in long-lived, shared caches that require it.
pub trait Entry:
    Default + Serialize + Copy + Clone + PartialEq + Eq + Ord + Hash + Debug + Send + Sync + 'static
{
    fn identifier(&self) -> i64;
}

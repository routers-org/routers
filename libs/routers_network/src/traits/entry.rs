use core::fmt::Debug;
use core::hash::Hash;
use serde::Serialize;

/// TODO: Description
pub trait Entry:
    Default + Serialize + Copy + Clone + PartialEq + Eq + Ord + Hash + Debug + Send + Sync
{
    fn identifier(&self) -> i64;
}

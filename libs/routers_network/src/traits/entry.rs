use core::hash::Hash;
use serde::Serialize;
use std::fmt::Debug;

/// TODO: Description
pub trait Entry:
    Default + Serialize + Copy + Clone + PartialEq + Eq + Ord + Hash + Debug + Send + Sync
{
    fn identifier(&self) -> i64;
}

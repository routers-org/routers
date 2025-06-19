use crate::Entry;
use std::fmt::Debug;

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Edge<E>
where
    E: Entry,
{
    pub source: E,
    pub target: E,
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Ord, Eq, Hash)]
#[repr(u8)]
pub enum Direction {
    Outgoing = 0,
    Incoming = 1,
}

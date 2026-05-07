use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Ord, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Direction {
    Outgoing = 0,
    Incoming = 1,
}

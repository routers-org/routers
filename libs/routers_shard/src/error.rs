use std::fmt::{Debug, Formatter};
use std::io;

pub enum ShardError {
    IOError(io::Error),
}

impl Debug for ShardError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ShardError::IOError(e) => format!("IOError: {e}"),
            }
        )
    }
}

impl From<io::Error> for ShardError {
    fn from(value: io::Error) -> ShardError {
        ShardError::IOError(value)
    }
}

//! Geometry utilities over a path of network nodes.
//!
//! A [`MapPath`] wraps an ordered list of nodes and answers geometric
//! questions about the path they trace: its length, its headings, and how
//! much turning it exhibits. The default transition costing uses these to
//! judge how plausible a candidate-to-candidate route is.

mod entity;

pub use entity::MapPath;

#[cfg(test)]
mod test;

#![doc = include_str!("../README.md")]
#![allow(dead_code)]
extern crate core;

pub mod graph;
pub mod transition;

#[cfg(test)]
pub mod test;

pub use graph::*;
pub use transition::*;

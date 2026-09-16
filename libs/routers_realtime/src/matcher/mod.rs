//! The regional matcher: load a graph, pull jobs, solve, publish results.
//!
//! A replica serves one `(graph, region)`, loading the pinned graph before it
//! touches the work queue and never claiming more work than it has slots to
//! solve. It owns no vehicle state and answers each
//! [`SolveJob`](crate::protocol::SolveJob) with one
//! [`SolveResult`](crate::protocol::SolveResult).

pub mod bootstrap;
pub mod engine;
pub mod publish;
pub mod pull;
pub mod validate;

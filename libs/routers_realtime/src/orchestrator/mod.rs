//! The partition orchestrator: vehicle continuity, admission, commit, recovery.
//!
//! An orchestrator pod owns a disjoint slice of the 1024 vehicle partitions, and
//! one [`worker`] task owns each partition outright: its `HashMap<VehicleId,
//! VehicleState>`, scheduler, frontier tracker, and deadline heap, with no
//! cross-partition sharing. The worker `select!`s over raw deliveries, result
//! deliveries, deadline ticks, and shutdown, driving each vehicle through its
//! state machine — read → schedule → admit → dispatch → validate → commit →
//! advance frontier.
//!
//! The modules realise the spec's orchestrator decomposition (spec §3); the
//! deadline handler and the result validator arbitrate the *same* per-vehicle
//! transition, and a prepared commit always wins over a later timeout:
//!
//! * [`reader`] — the ordered raw reader: validate envelope/subject identity,
//!   suppress committed and already-pending duplicates.
//! * [`scheduler`] — per-vehicle FIFO, ready queue, one active logical job;
//!   never evicts in-flight or committing state.
//! * [`admission`] — caps outstanding jobs and bytes globally and per region,
//!   holding waiting work outside the matchers' CPU queues.
//! * [`dispatch`] — the job builder/publisher: deterministic immutable context
//!   and [`JobId`](crate::protocol::JobId), published with acknowledgement and
//!   retried byte-identically on an ambiguous send.
//! * [`validate`] — the result validator: accept/park/reject/quarantine against
//!   the expected job, base state, segment, and version; enforce finality.
//! * [`commit`] — the commit coordinator: prepare → publish → promote → complete,
//!   and re-drive a prepared commit after a crash.
//! * [`frontier`] — the completion-frontier tracker: advance only the contiguous
//!   completed prefix of the partition's input.
//! * [`deadline`] — the deadline and terminal-outcome handler: close expired
//!   untouched work through the same commit path and make late results ineligible.
//! * [`recovery`] — the recovery coordinator: resolve surviving prepared commits,
//!   resume ordered replay after the frontier, reconstruct expected jobs.
//! * [`worker`] — the per-partition event loop that wires the above together.
//!
//! Draining is handled by [`lifecycle`](crate::lifecycle).

pub mod admission;
pub mod commit;
pub mod deadline;
pub mod dispatch;
pub mod frontier;
pub mod reader;
pub mod recovery;
pub mod scheduler;
pub mod validate;
pub mod worker;

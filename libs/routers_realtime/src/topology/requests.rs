//! Transient solve-request plane.
//!
//! Requests are Core NATS messages addressed by `(graph version, region, lane)`
//! on `solve.req.v1.g.<graph>.r.<region>.q.<lane>`, queue-grouped across the
//! region's matchers. A matcher also listens on its own replica subject, where
//! an owner steers a vehicle whose trip that replica holds. Answers return to
//! the owning orchestrator's process inbox, one subject per partition. Nothing
//! on this plane is durable: the unacknowledged raw event is the retry source.

use crate::protocol::ids::{GraphVersion, Lane, RegionId};

/// The subject prefix every solve request shares.
pub const REQUEST_PREFIX: &str = "solve.req.v1";

/// The queue-group subject for a request on `lane`:
/// `solve.req.v1.g.<graph>.r.<region>.q.<lane>`.
pub fn request_subject(graph: &GraphVersion, region: &RegionId, lane: Lane) -> String {
    format!("{REQUEST_PREFIX}.g.{graph}.r.{region}.q.{}", lane.0)
}

/// What a region's matchers queue-subscribe to: every lane of their graph.
pub fn request_queue_filter(graph: &GraphVersion, region: &RegionId) -> String {
    format!("{REQUEST_PREFIX}.g.{graph}.r.{region}.q.*")
}

/// One matcher replica's own request subject, outside the queue group:
/// `solve.req.v1.g.<graph>.r.<region>.m.<replica>`.
pub fn sticky_subject(graph: &GraphVersion, region: &RegionId, replica: &str) -> String {
    format!("{REQUEST_PREFIX}.g.{graph}.r.{region}.m.{replica}")
}

/// The inbox subject an owner receives `partition`'s answers on.
pub fn reply_subject(prefix: &str, partition: u16) -> String {
    format!("{prefix}.p.{partition}")
}

/// Everything addressed to one owner process's inbox.
pub fn reply_filter(prefix: &str) -> String {
    format!("{prefix}.p.*")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::partition_of_subject;

    fn graph() -> GraphVersion {
        GraphVersion::new("g1").unwrap()
    }

    fn region() -> RegionId {
        RegionId::new("syd").unwrap()
    }

    #[test]
    fn request_subjects_share_the_queue_filter_but_not_the_replica_subject() {
        let queued = request_subject(&graph(), &region(), Lane(2));
        assert_eq!(queued, "solve.req.v1.g.g1.r.syd.q.2");
        assert_eq!(
            request_queue_filter(&graph(), &region()),
            "solve.req.v1.g.g1.r.syd.q.*"
        );
        assert_eq!(
            sticky_subject(&graph(), &region(), "abc"),
            "solve.req.v1.g.g1.r.syd.m.abc"
        );
    }

    #[test]
    fn reply_subjects_address_a_partition() {
        let subject = reply_subject("_INBOX.owner", 485);
        assert_eq!(subject, "_INBOX.owner.p.485");
        assert_eq!(partition_of_subject(&subject), Some(485));
        assert_eq!(reply_filter("_INBOX.owner"), "_INBOX.owner.p.*");
    }
}

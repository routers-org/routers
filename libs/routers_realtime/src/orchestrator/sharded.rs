//! Bounded internal routing for a broker consumer shared by several partitions.
//!
//! A router never acknowledges work. It moves each delivery, with its unique
//! acknowledgement handle, to the one partition worker that owns its subject.
//! If that worker cannot receive it, the handle remains broker-owned for a
//! retry. This keeps broker sharding an implementation detail: workers retain
//! their existing single-task ordering and commit semantics.

use core::num::NonZeroUsize;
use std::collections::HashMap;

use alloc::collections::VecDeque;
use anyhow::Context as _;
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt as _};
use tokio::sync::mpsc;

use crate::bus::Wire;
use crate::bus::adapter::{AckHandle, Delivery, Source};
use crate::lifecycle::{DrainReason, Shutdown};
use crate::topology::partition_of_subject;

/// A partition-local end of a bounded sharded-consumer route.
pub struct PartitionSource<T, H: AckHandle> {
    receiver: mpsc::Receiver<Delivery<T, H>>,
}

/// Partition id to the sending side of its bounded route.
pub type PartitionRoutes<T, H> = HashMap<u16, mpsc::Sender<Delivery<T, H>>>;

/// Partition id to the worker-owned receiving side of its bounded route.
pub type PartitionSources<T, H> = HashMap<u16, PartitionSource<T, H>>;

type RoutePermit<T, H> = Result<mpsc::OwnedPermit<Delivery<T, H>>, mpsc::error::SendError<()>>;
type RouteReservation<T, H> = BoxFuture<'static, (u16, RoutePermit<T, H>)>;

impl<T: Wire + Send, H: AckHandle> Source<T> for PartitionSource<T, H> {
    type Handle = H;

    async fn next(&mut self) -> Option<anyhow::Result<Delivery<T, Self::Handle>>> {
        self.receiver.recv().await.map(Ok)
    }
}

/// Build one bounded route for every partition. A separate source is handed to
/// each worker, so it is impossible for two workers to read the same queue.
pub fn partition_routes<T, H: AckHandle>(
    partitions: impl IntoIterator<Item = u16>,
    capacity: NonZeroUsize,
) -> (PartitionRoutes<T, H>, PartitionSources<T, H>) {
    let mut senders = HashMap::new();
    let mut sources = HashMap::new();
    for partition in partitions {
        let (sender, receiver) = mpsc::channel(capacity.get());
        assert!(
            senders.insert(partition, sender).is_none(),
            "duplicate partition route"
        );
        assert!(
            sources
                .insert(partition, PartitionSource { receiver })
                .is_none(),
            "duplicate partition source"
        );
    }
    (senders, sources)
}

/// Forward a shared durable's deliveries to its partition-local worker queues.
///
/// A busy partition is buffered independently, so it cannot block deliveries
/// for the other partitions owned by this router. Both the worker channels and
/// router-owned pending deliveries are bounded. A failed/closed route
/// negatively acknowledges its next delivery and forces a process drain;
/// continuing would strand a durable on a replica that no longer owns every
/// one of its partitions.
pub async fn route<T, Src>(
    mut source: Src,
    routes: PartitionRoutes<T, Src::Handle>,
    shutdown: Shutdown,
) -> anyhow::Result<()>
where
    T: Wire + Send + 'static,
    Src: Source<T>,
{
    let pending_limit = routes
        .values()
        .map(mpsc::Sender::max_capacity)
        .sum::<usize>()
        .max(1);
    let mut pending = HashMap::<u16, VecDeque<Delivery<T, Src::Handle>>>::new();
    let mut pending_count = 0_usize;
    let mut reservations: FuturesUnordered<RouteReservation<T, Src::Handle>> =
        FuturesUnordered::new();

    loop {
        tokio::select! {
            _ = shutdown.triggered() => return Ok(()),
            reservation = reservations.next(), if !reservations.is_empty() => {
                let (partition, permit) = reservation.expect("guarded by non-empty check");
                let queue = pending
                    .get_mut(&partition)
                    .expect("a reservation always has pending work");
                let delivery = queue.pop_front().expect("pending queue is never empty");
                pending_count -= 1;
                let has_more = !queue.is_empty();

                match permit {
                    Ok(permit) => {
                        permit.send(delivery);
                    }
                    Err(_) => {
                        let subject = delivery.subject.clone();
                        let _ = delivery.handle.nak(None).await;
                        shutdown.trigger(DrainReason::Fatal);
                        anyhow::bail!("partition route for {subject} closed");
                    }
                }

                if has_more {
                    let sender = routes
                        .get(&partition)
                        .expect("pending work belongs to an existing route")
                        .clone();
                    reservations.push(reserve_route(partition, sender));
                } else {
                    pending.remove(&partition);
                }
            }
            next = source.next(), if pending_count < pending_limit => {
                let Some(delivery) = next else {
                    if shutdown.is_triggered() {
                        return Ok(());
                    }
                    shutdown.trigger(DrainReason::Fatal);
                    anyhow::bail!("sharded source closed before shutdown");
                };
                let delivery = match delivery {
                    Ok(delivery) => delivery,
                    Err(error) => {
                        shutdown.trigger(DrainReason::Fatal);
                        return Err(error).context("sharded JetStream source failed");
                    }
                };
                let Some(partition) = partition_of_subject(&delivery.subject) else {
                    let subject = delivery.subject.clone();
                    let _ = delivery.handle.nak(None).await;
                    shutdown.trigger(DrainReason::Fatal);
                    anyhow::bail!("shared consumer delivered unroutable subject {subject}");
                };
                let Some(sender) = routes.get(&partition) else {
                    let subject = delivery.subject.clone();
                    let _ = delivery.handle.nak(None).await;
                    shutdown.trigger(DrainReason::Fatal);
                    anyhow::bail!("shared consumer delivered unroutable subject {subject}");
                };

                if let Some(queue) = pending.get_mut(&partition) {
                    queue.push_back(delivery);
                    pending_count += 1;
                    continue;
                }

                match sender.try_send(delivery) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(delivery)) => {
                        pending.insert(partition, VecDeque::from([delivery]));
                        pending_count += 1;
                        reservations.push(reserve_route(partition, sender.clone()));
                    }
                    Err(mpsc::error::TrySendError::Closed(delivery)) => {
                        let subject = delivery.subject.clone();
                        let _ = delivery.handle.nak(None).await;
                        shutdown.trigger(DrainReason::Fatal);
                        anyhow::bail!("partition route for {subject} closed");
                    }
                }
            }
        }
    }
}

fn reserve_route<T: Send + 'static, H: AckHandle>(
    partition: u16,
    sender: mpsc::Sender<Delivery<T, H>>,
) -> RouteReservation<T, H> {
    Box::pin(async move { (partition, sender.reserve_owned().await) })
}

/// The earliest raw stream position a shared durable may safely resume from.
///
/// A missing member frontier means that partition has no store-backed replay
/// seam, so the whole shared consumer binds at the stream head. Otherwise the
/// earliest successor is used and workers whose frontiers are further ahead
/// suppress the harmless replay locally.
#[must_use]
pub fn raw_replay_start(frontiers: impl IntoIterator<Item = Option<u64>>) -> Option<u64> {
    let mut earliest = None;
    for frontier in frontiers {
        let next = frontier?.saturating_add(1);
        earliest = Some(earliest.map_or(next, |current: u64| current.min(next)));
    }
    earliest
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use alloc::sync::Arc;
    use core::time::Duration;
    use std::sync::Mutex;

    use async_nats::HeaderMap;

    use super::*;
    use crate::bus::adapter::AckHandle;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestWire(u8);

    impl Wire for TestWire {
        fn encode(&self) -> anyhow::Result<Vec<u8>> {
            Ok(vec![self.0])
        }
        fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
            Ok(Self(bytes[0]))
        }
    }

    #[derive(Clone)]
    struct TestAck {
        sequence: u64,
        operations: Arc<Mutex<Vec<&'static str>>>,
    }

    impl AckHandle for TestAck {
        async fn ack(self) -> anyhow::Result<()> {
            self.operations.lock().unwrap().push("ack");
            Ok(())
        }
        async fn nak(self, _: Option<Duration>) -> anyhow::Result<()> {
            self.operations.lock().unwrap().push("nak");
            Ok(())
        }
        fn sequence(&self) -> u64 {
            self.sequence
        }
        fn deliveries(&self) -> u32 {
            1
        }
    }

    struct TestSource {
        deliveries: VecDeque<Delivery<TestWire, TestAck>>,
    }

    impl Source<TestWire> for TestSource {
        type Handle = TestAck;
        async fn next(&mut self) -> Option<anyhow::Result<Delivery<TestWire, Self::Handle>>> {
            self.deliveries.pop_front().map(Ok)
        }
    }

    struct OpenTestSource(TestSource);

    impl Source<TestWire> for OpenTestSource {
        type Handle = TestAck;
        async fn next(&mut self) -> Option<anyhow::Result<Delivery<TestWire, Self::Handle>>> {
            if let Some(delivery) = self.0.deliveries.pop_front() {
                return Some(Ok(delivery));
            }
            core::future::pending().await
        }
    }

    fn delivery(partition: u16, value: u8, ack: TestAck) -> Delivery<TestWire, TestAck> {
        Delivery {
            item: TestWire(value),
            handle: ack,
            subject: format!("events.raw.p.{partition}"),
            msg_id: None,
            headers: HeaderMap::new(),
            sent_at: None,
            redelivered: false,
        }
    }

    #[tokio::test]
    async fn routing_is_partition_owned_and_keeps_each_queue_ordered() {
        let ops = Arc::new(Mutex::new(Vec::new()));
        let (routes, mut sources) = partition_routes([2, 3], NonZeroUsize::new(4).unwrap());
        let source = TestSource {
            deliveries: [
                delivery(
                    2,
                    1,
                    TestAck {
                        sequence: 1,
                        operations: ops.clone(),
                    },
                ),
                delivery(
                    3,
                    9,
                    TestAck {
                        sequence: 2,
                        operations: ops.clone(),
                    },
                ),
                delivery(
                    2,
                    2,
                    TestAck {
                        sequence: 3,
                        operations: ops.clone(),
                    },
                ),
            ]
            .into(),
        };
        let shutdown = Shutdown::new();
        assert!(route(source, routes, shutdown.clone()).await.is_err());
        assert_eq!(shutdown.reason(), Some(DrainReason::Fatal));

        let two = sources.get_mut(&2).unwrap();
        assert_eq!(two.next().await.unwrap().unwrap().item, TestWire(1));
        assert_eq!(two.next().await.unwrap().unwrap().item, TestWire(2));
        assert_eq!(
            sources
                .get_mut(&3)
                .unwrap()
                .next()
                .await
                .unwrap()
                .unwrap()
                .item,
            TestWire(9)
        );
        assert!(ops.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn closed_owner_naks_instead_of_losing_the_delivery() {
        let ops = Arc::new(Mutex::new(Vec::new()));
        let (mut routes, sources) = partition_routes([2], NonZeroUsize::new(1).unwrap());
        drop(sources);
        let source = TestSource {
            deliveries: [delivery(
                2,
                1,
                TestAck {
                    sequence: 1,
                    operations: ops.clone(),
                },
            )]
            .into(),
        };
        assert!(
            route(source, routes.clone(), Shutdown::new())
                .await
                .is_err()
        );
        assert_eq!(*ops.lock().unwrap(), ["nak"]);
        routes.clear();
    }

    #[tokio::test]
    async fn a_full_partition_does_not_block_another_partition() {
        let ops = Arc::new(Mutex::new(Vec::new()));
        let (routes, mut sources) = partition_routes([2, 3], NonZeroUsize::new(1).unwrap());
        let source = OpenTestSource(TestSource {
            deliveries: [
                delivery(
                    2,
                    1,
                    TestAck {
                        sequence: 1,
                        operations: ops.clone(),
                    },
                ),
                delivery(
                    2,
                    2,
                    TestAck {
                        sequence: 2,
                        operations: ops.clone(),
                    },
                ),
                delivery(
                    3,
                    9,
                    TestAck {
                        sequence: 3,
                        operations: ops.clone(),
                    },
                ),
            ]
            .into(),
        });
        let shutdown = Shutdown::new();
        let route_task = tokio::spawn(route(source, routes, shutdown.clone()));

        let three =
            tokio::time::timeout(Duration::from_secs(1), sources.get_mut(&3).unwrap().next())
                .await
                .expect("partition 3 was blocked by full partition 2")
                .unwrap()
                .unwrap();
        assert_eq!(three.item, TestWire(9));

        let two = sources.get_mut(&2).unwrap();
        assert_eq!(two.next().await.unwrap().unwrap().item, TestWire(1));
        assert_eq!(two.next().await.unwrap().unwrap().item, TestWire(2));

        shutdown.trigger(DrainReason::Operator);
        route_task.await.unwrap().unwrap();
        assert!(ops.lock().unwrap().is_empty());
    }

    #[test]
    fn raw_replay_starts_at_the_earliest_member_frontier() {
        assert_eq!(raw_replay_start([Some(20), Some(7), Some(30)]), Some(8));
        assert_eq!(raw_replay_start([Some(u64::MAX)]), Some(u64::MAX));
        assert_eq!(raw_replay_start([Some(7), None, Some(20)]), None);
        assert_eq!(raw_replay_start([]), None);
    }

    #[tokio::test]
    async fn source_closure_forces_a_fatal_drain() {
        let (routes, _sources) = partition_routes([2], NonZeroUsize::new(1).unwrap());
        let shutdown = Shutdown::new();
        let source = TestSource {
            deliveries: VecDeque::new(),
        };

        assert!(route(source, routes, shutdown.clone()).await.is_err());
        assert_eq!(shutdown.reason(), Some(DrainReason::Fatal));
    }
}

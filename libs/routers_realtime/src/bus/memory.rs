//! In-memory bus for tests: a faithful fake of the JetStream mechanics the
//! adapter traits in [`super::adapter`] describe.
//!
//! [`MemoryBus`] reproduces `Nats-Msg-Id` dedup, independent per-consumer
//! cursors, redelivery of unacked messages, and ambiguous publish. It is not a
//! network: ordering is total and delivery is immediate.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::marker::PhantomData;
use core::time::Duration;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_nats::HeaderMap;
use tokio::sync::Notify;
use tokio::time::Instant;
use web_time::{SystemTime, UNIX_EPOCH};

use crate::bus::Wire;
use crate::bus::adapter::{
    AckHandle, Consumer, Delivery, PublishError, PublishOutcome, Publisher, Source,
};
use crate::protocol::ids::headers::SENT_AT_MS;

/// One published record as returned by [`MemoryBus::published`]: the concrete
/// subject, the dedup key if one was set, and the raw bytes.
pub type PublishedRecord = (String, Option<String>, Vec<u8>);

/// A stored message: the durable facts a stream keeps about one publish.
struct Stored {
    /// Stream sequence, assigned densely from 1 (so `seq == index + 1`).
    seq: u64,
    /// The concrete subject it was published to.
    subject: String,
    /// The `Nats-Msg-Id`, if one was set.
    msg_id: Option<String>,
    /// The published headers — the send time is read back from these.
    headers: HeaderMap,
    /// The message body.
    bytes: Vec<u8>,
}

/// One consumer's private progress over the shared log — its own cursor and its
/// own ack/redelivery bookkeeping, mirroring an independent durable consumer.
struct ConsumerState {
    /// The subject filter this consumer subscribes to.
    filter: String,
    /// Index of the next never-before-seen log entry to consider.
    cursor: usize,
    /// How many times each sequence has been delivered to this consumer.
    delivered: HashMap<u64, u32>,
    /// Sequences this consumer has acknowledged.
    acked: HashSet<u64>,
    /// Sequences queued for redelivery, served ahead of new messages.
    redeliver: VecDeque<u64>,
}

/// The shared, lock-guarded state of the bus.
struct Inner {
    /// The total-ordered log across every subject.
    log: Vec<Stored>,
    /// Next stream sequence to assign.
    next_seq: u64,
    /// Dedup index: `(stream group, msg_id) -> stored sequence`.
    dedup: HashMap<(String, String), u64>,
    /// Registered consumers by id.
    consumers: HashMap<u64, ConsumerState>,
    /// Next consumer id to hand out.
    next_consumer_id: u64,
    /// Set once [`MemoryBus::close`] is called; sources then drain to `None`.
    closed: bool,
    /// A one-shot publish failure armed by [`MemoryBus::fail_next_publish`].
    fail_next: Option<PublishError>,
    /// Every nak backoff hint seen, in order; recorded but never honoured.
    nak_delays: Vec<Option<Duration>>,
    /// Wakes parked sources/consumers when new work becomes available.
    notify: Arc<Notify>,
}

/// The result of polling one deliverable message out of the log.
struct Polled {
    seq: u64,
    subject: String,
    msg_id: Option<String>,
    headers: HeaderMap,
    bytes: Vec<u8>,
    sent_at: Option<SystemTime>,
    redelivered: bool,
    deliveries: u32,
}

impl Polled {
    fn build(stored: &Stored, redelivered: bool, deliveries: u32) -> Self {
        Polled {
            seq: stored.seq,
            subject: stored.subject.clone(),
            msg_id: stored.msg_id.clone(),
            headers: stored.headers.clone(),
            bytes: stored.bytes.clone(),
            sent_at: sent_at_from(&stored.headers),
            redelivered,
            deliveries,
        }
    }
}

impl Inner {
    /// Store a message (or recognise a duplicate) and report the outcome.
    fn store(
        &mut self,
        subject: &str,
        msg_id: &str,
        headers: &HeaderMap,
        bytes: &[u8],
    ) -> (u64, bool) {
        let dedup_key = (!msg_id.is_empty()).then(|| (subject_group(subject), msg_id.to_owned()));
        if let Some(key) = &dedup_key
            && let Some(&existing) = self.dedup.get(key)
        {
            return (existing, true);
        }

        let seq = self.next_seq;
        self.next_seq += 1;
        self.log.push(Stored {
            seq,
            subject: subject.to_owned(),
            msg_id: (!msg_id.is_empty()).then(|| msg_id.to_owned()),
            headers: headers.clone(),
            bytes: bytes.to_vec(),
        });
        if let Some(key) = dedup_key {
            self.dedup.insert(key, seq);
        }
        (seq, false)
    }

    fn try_publish(
        &mut self,
        subject: &str,
        msg_id: &str,
        headers: &HeaderMap,
        bytes: &[u8],
    ) -> Result<PublishOutcome, PublishError> {
        if let Some(err) = self.fail_next.take() {
            return match err {
                // Ambiguous stores the message so a retry dedups; Failed stores nothing.
                PublishError::Ambiguous(cause) => {
                    self.store(subject, msg_id, headers, bytes);
                    Err(PublishError::Ambiguous(cause))
                }
                PublishError::Failed(cause) => Err(PublishError::Failed(cause)),
            };
        }
        let (sequence, duplicate) = self.store(subject, msg_id, headers, bytes);
        Ok(PublishOutcome::Acked {
            sequence,
            duplicate,
        })
    }

    /// Take the next message this consumer should see: a queued redelivery
    /// first, then the next new matching message from its cursor.
    fn poll(&mut self, consumer_id: u64) -> Option<Polled> {
        let log = &self.log;
        let cs = self.consumers.get_mut(&consumer_id)?;

        while let Some(seq) = cs.redeliver.pop_front() {
            if cs.acked.contains(&seq) {
                continue;
            }
            if let Some(stored) = log.get(seq.wrapping_sub(1) as usize) {
                let count = cs.delivered.entry(seq).or_insert(0);
                *count += 1;
                return Some(Polled::build(stored, true, *count));
            }
        }

        while let Some(stored) = log.get(cs.cursor) {
            cs.cursor += 1;
            if subject_matches(&cs.filter, &stored.subject) {
                let count = cs.delivered.entry(stored.seq).or_insert(0);
                *count += 1;
                return Some(Polled::build(stored, false, *count));
            }
        }
        None
    }

    fn register_consumer(&mut self, filter: &str) -> u64 {
        let id = self.next_consumer_id;
        self.next_consumer_id += 1;
        self.consumers.insert(
            id,
            ConsumerState {
                filter: filter.to_owned(),
                cursor: 0,
                delivered: HashMap::new(),
                acked: HashSet::new(),
                redeliver: VecDeque::new(),
            },
        );
        id
    }

    fn ack(&mut self, consumer_id: u64, seq: u64) {
        if let Some(cs) = self.consumers.get_mut(&consumer_id) {
            cs.acked.insert(seq);
            cs.redeliver.retain(|&queued| queued != seq);
        }
    }

    fn nak(&mut self, consumer_id: u64, seq: u64, delay: Option<Duration>) {
        self.nak_delays.push(delay);
        self.queue_redelivery(consumer_id, seq);
    }

    fn queue_redelivery(&mut self, consumer_id: u64, seq: u64) {
        if let Some(cs) = self.consumers.get_mut(&consumer_id)
            && !cs.acked.contains(&seq)
            && !cs.redeliver.contains(&seq)
        {
            cs.redeliver.push_back(seq);
        }
    }

    fn redeliver_unacked(&mut self, filter: &str) {
        let log = &self.log;
        for cs in self.consumers.values_mut() {
            let mut to_queue: Vec<u64> = Vec::new();
            for (&seq, &count) in &cs.delivered {
                if count > 0
                    && !cs.acked.contains(&seq)
                    && !cs.redeliver.contains(&seq)
                    && log
                        .get(seq.wrapping_sub(1) as usize)
                        .is_some_and(|stored| subject_matches(filter, &stored.subject))
                {
                    to_queue.push(seq);
                }
            }
            // Deterministic redelivery order regardless of hash iteration.
            to_queue.sort_unstable();
            for seq in to_queue {
                cs.redeliver.push_back(seq);
            }
        }
    }
}

/// An in-memory fake of the durable bus. Cheap to [`Clone`]: every clone shares
/// the same log and consumers through an [`Arc`].
#[derive(Clone)]
pub struct MemoryBus(Arc<Mutex<Inner>>);

impl Default for MemoryBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBus {
    /// A fresh, empty bus.
    #[must_use]
    pub fn new() -> Self {
        MemoryBus(Arc::new(Mutex::new(Inner {
            log: Vec::new(),
            next_seq: 1,
            dedup: HashMap::new(),
            consumers: HashMap::new(),
            next_consumer_id: 0,
            closed: false,
            fail_next: None,
            nak_delays: Vec::new(),
            notify: Arc::new(Notify::new()),
        })))
    }

    /// A publisher for messages of type `T`.
    #[must_use]
    pub fn publisher<T: Wire>(&self) -> MemoryPublisher<T> {
        MemoryPublisher {
            bus: self.clone(),
            _marker: PhantomData,
        }
    }

    /// A push-style [`Source`] over messages matching `filter`, with its own
    /// cursor.
    #[must_use]
    pub fn source<T: Wire>(&self, filter: &str) -> MemorySource<T> {
        let consumer_id = self.0.lock().unwrap().register_consumer(filter);
        MemorySource {
            bus: self.clone(),
            consumer_id,
            _marker: PhantomData,
        }
    }

    /// A pull-style [`Consumer`] over messages matching `filter`, with its own
    /// cursor.
    #[must_use]
    pub fn consumer<T: Wire>(&self, filter: &str) -> MemoryConsumer<T> {
        let consumer_id = self.0.lock().unwrap().register_consumer(filter);
        MemoryConsumer {
            bus: self.clone(),
            consumer_id,
            _marker: PhantomData,
        }
    }

    /// Arm a one-shot publish failure returned by the *next* publish. An
    /// [`PublishError::Ambiguous`] still stores the message (so a retry dedups);
    /// a [`PublishError::Failed`] stores nothing.
    pub fn fail_next_publish(&self, err: PublishError) {
        self.0.lock().unwrap().fail_next = Some(err);
    }

    /// Mark every delivered-but-unacked message matching `filter` for
    /// redelivery with `redelivered = true`. Acked messages are untouched.
    pub fn redeliver_unacked(&self, filter: &str) {
        let notify = {
            let mut inner = self.0.lock().unwrap();
            inner.redeliver_unacked(filter);
            inner.notify.clone()
        };
        notify.notify_waiters();
    }

    /// Every message stored under a subject matching `filter`, in stream order.
    #[must_use]
    pub fn published(&self, filter: &str) -> Vec<PublishedRecord> {
        let inner = self.0.lock().unwrap();
        inner
            .log
            .iter()
            .filter(|stored| subject_matches(filter, &stored.subject))
            .map(|stored| {
                (
                    stored.subject.clone(),
                    stored.msg_id.clone(),
                    stored.bytes.clone(),
                )
            })
            .collect()
    }

    /// How many acknowledgements have landed for messages matching `filter`,
    /// summed across every consumer.
    #[must_use]
    pub fn acked_count(&self, filter: &str) -> usize {
        let inner = self.0.lock().unwrap();
        inner
            .consumers
            .values()
            .flat_map(|cs| cs.acked.iter())
            .filter(|&&seq| {
                inner
                    .log
                    .get(seq.wrapping_sub(1) as usize)
                    .is_some_and(|stored| subject_matches(filter, &stored.subject))
            })
            .count()
    }

    /// The nak backoff hints recorded so far, in order.
    #[must_use]
    pub fn nak_delays(&self) -> Vec<Option<Duration>> {
        self.0.lock().unwrap().nak_delays.clone()
    }

    /// Empty the log and every consumer's progress, keeping registrations.
    pub fn clear(&self) {
        let notify = {
            let mut inner = self.0.lock().unwrap();
            inner.log.clear();
            inner.dedup.clear();
            inner.next_seq = 1;
            for cs in inner.consumers.values_mut() {
                cs.cursor = 0;
                cs.delivered.clear();
                cs.acked.clear();
                cs.redeliver.clear();
            }
            inner.notify.clone()
        };
        notify.notify_waiters();
    }

    /// Close the bus: parked sources drain what is left and then return `None`.
    pub fn close(&self) {
        let notify = {
            let mut inner = self.0.lock().unwrap();
            inner.closed = true;
            inner.notify.clone()
        };
        notify.notify_waiters();
    }

    fn notifier(&self) -> Arc<Notify> {
        self.0.lock().unwrap().notify.clone()
    }

    fn publish_stored(
        &self,
        subject: &str,
        msg_id: &str,
        headers: &HeaderMap,
        bytes: &[u8],
    ) -> Result<PublishOutcome, PublishError> {
        let (result, notify) = {
            let mut inner = self.0.lock().unwrap();
            let result = inner.try_publish(subject, msg_id, headers, bytes);
            (result, inner.notify.clone())
        };
        notify.notify_waiters();
        result
    }

    /// Decode one claimed record. A malformed record is transport poison, just
    /// like it is in JetStream: acknowledge it here and keep looking for a
    /// useful delivery. There is no handle a caller could safely use after
    /// decoding failed, so returning an error would leave a record unretired.
    fn deliver<T: Wire>(&self, consumer_id: u64, polled: Polled) -> Option<Delivery<T, MemoryAck>> {
        let item = match T::decode(&polled.bytes) {
            Ok(item) => item,
            Err(error) => {
                self.0.lock().unwrap().ack(consumer_id, polled.seq);
                tracing::warn!(subject = %polled.subject, %error, "acking undecodable memory-bus message");
                return None;
            }
        };
        Some(Delivery {
            item,
            handle: MemoryAck {
                bus: self.clone(),
                consumer_id,
                seq: polled.seq,
                deliveries: polled.deliveries,
            },
            subject: polled.subject,
            msg_id: polled.msg_id,
            headers: polled.headers,
            sent_at: polled.sent_at,
            redelivered: polled.redelivered,
        })
    }
}

/// The acknowledgement handle the memory bus hands out with each delivery.
pub struct MemoryAck {
    bus: MemoryBus,
    consumer_id: u64,
    seq: u64,
    deliveries: u32,
}

impl AckHandle for MemoryAck {
    async fn ack(self) -> anyhow::Result<()> {
        self.bus.0.lock().unwrap().ack(self.consumer_id, self.seq);
        Ok(())
    }

    async fn nak(self, delay: Option<Duration>) -> anyhow::Result<()> {
        if let Some(delay) = delay.filter(|delay| !delay.is_zero()) {
            let bus = self.bus.clone();
            let consumer_id = self.consumer_id;
            let seq = self.seq;
            {
                let mut inner = bus.0.lock().unwrap();
                inner.nak_delays.push(Some(delay));
            }
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let notify = {
                    let mut inner = bus.0.lock().unwrap();
                    inner.queue_redelivery(consumer_id, seq);
                    inner.notify.clone()
                };
                notify.notify_waiters();
            });
            return Ok(());
        }
        let notify = {
            let mut inner = self.bus.0.lock().unwrap();
            inner.nak(self.consumer_id, self.seq, delay);
            inner.notify.clone()
        };
        notify.notify_waiters();
        Ok(())
    }

    fn sequence(&self) -> u64 {
        self.seq
    }

    fn deliveries(&self) -> u32 {
        self.deliveries
    }
}

/// A [`Publisher`] over the memory bus.
pub struct MemoryPublisher<T> {
    bus: MemoryBus,
    _marker: PhantomData<fn() -> T>,
}

// Manual `Clone` so `T` need not be `Clone` (the marker is a function pointer).
impl<T> Clone for MemoryPublisher<T> {
    fn clone(&self) -> Self {
        MemoryPublisher {
            bus: self.bus.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T: Wire + 'static> Publisher<T> for MemoryPublisher<T> {
    async fn publish_bytes(
        &self,
        subject: &str,
        msg_id: &str,
        headers: HeaderMap,
        bytes: &[u8],
    ) -> Result<PublishOutcome, PublishError> {
        self.bus.publish_stored(subject, msg_id, &headers, bytes)
    }
}

/// A push-style [`Source`] over the memory bus.
pub struct MemorySource<T> {
    bus: MemoryBus,
    consumer_id: u64,
    _marker: PhantomData<fn() -> T>,
}

impl<T: Wire> Source<T> for MemorySource<T> {
    type Handle = MemoryAck;

    async fn next(&mut self) -> Option<anyhow::Result<Delivery<T, MemoryAck>>> {
        let notify = self.bus.notifier();
        loop {
            // Arm the waiter before checking, so a racing publish still wakes us.
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let polled = {
                let mut inner = self.bus.0.lock().unwrap();
                match inner.poll(self.consumer_id) {
                    Some(polled) => Some(polled),
                    None if inner.closed => return None,
                    None => None,
                }
            };
            if let Some(polled) = polled {
                if let Some(delivery) = self.bus.deliver::<T>(self.consumer_id, polled) {
                    return Some(Ok(delivery));
                }
                continue;
            }

            notified.as_mut().await;
        }
    }
}

/// A pull-style [`Consumer`] over the memory bus.
pub struct MemoryConsumer<T> {
    bus: MemoryBus,
    consumer_id: u64,
    _marker: PhantomData<fn() -> T>,
}

impl<T: Wire> Consumer<T> for MemoryConsumer<T> {
    type Handle = MemoryAck;

    async fn fetch(
        &mut self,
        max: usize,
        wait: Duration,
    ) -> anyhow::Result<Vec<Delivery<T, MemoryAck>>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let notify = self.bus.notifier();
        let deadline = Instant::now() + wait;
        loop {
            // Arm before draining, for the same lost-wakeup reason as `next`.
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let (polled, closed) = {
                let mut inner = self.bus.0.lock().unwrap();
                let mut drained = Vec::new();
                while drained.len() < max {
                    match inner.poll(self.consumer_id) {
                        Some(item) => drained.push(item),
                        None => break,
                    }
                }
                (drained, inner.closed)
            };

            if !polled.is_empty() {
                let mut batch = Vec::with_capacity(polled.len());
                for item in polled {
                    if let Some(delivery) = self.bus.deliver::<T>(self.consumer_id, item) {
                        batch.push(delivery);
                    }
                }
                return Ok(batch);
            }
            if closed {
                return Ok(Vec::new());
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(Vec::new());
            }
            let _ = tokio::time::timeout(remaining, notified.as_mut()).await;
        }
    }
}

/// The dedup namespace of a subject: everything up to its final token, so
/// sibling subjects in one stream dedup a repeated `Nats-Msg-Id` across them.
fn subject_group(subject: &str) -> String {
    match subject.rfind('.') {
        Some(dot) => subject[..dot].to_owned(),
        None => subject.to_owned(),
    }
}

/// Read the producer's send time from the `x-routers-sent-at-ms` header.
fn sent_at_from(headers: &HeaderMap) -> Option<SystemTime> {
    let millis: u64 = headers.get(SENT_AT_MS)?.as_str().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

/// Whether `subject` matches NATS subject `filter`. Supports an exact match, a
/// single-token `*` wildcard in any position, and a trailing `>` that matches
/// one or more remaining tokens.
fn subject_matches(filter: &str, subject: &str) -> bool {
    let filter_tokens: Vec<&str> = filter.split('.').collect();
    let subject_tokens: Vec<&str> = subject.split('.').collect();

    for (index, token) in filter_tokens.iter().enumerate() {
        match *token {
            ">" => return index + 1 == filter_tokens.len() && subject_tokens.len() > index,
            "*" => {
                if index >= subject_tokens.len() {
                    return false;
                }
            }
            literal => {
                if subject_tokens.get(index) != Some(&literal) {
                    return false;
                }
            }
        }
    }
    subject_tokens.len() == filter_tokens.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::postcard_wire;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct TestMsg {
        n: u64,
        label: String,
    }
    postcard_wire!(TestMsg);

    fn msg(n: u64) -> TestMsg {
        TestMsg {
            n,
            label: format!("m{n}"),
        }
    }

    #[test]
    fn subject_matching_table() {
        let cases = [
            ("a.b.c", "a.b.c", true),
            ("a.b.c", "a.b.d", false),
            ("a.b.c", "a.b", false),
            ("a.b", "a.b.c", false),
            ("a.b.>", "a.b.c", true),
            ("a.b.>", "a.b.c.d", true),
            ("a.b.>", "a.b", false),
            (">", "a", true),
            (">", "a.b.c", true),
            ("a.*.c", "a.b.c", true),
            ("a.*.c", "a.x.c", true),
            ("a.*.c", "a.b.d", false),
            ("a.*", "a.b", true),
            ("a.*", "a.b.c", false),
            ("*.b", "a.b", true),
            ("solve-result.v1.p.>", "solve-result.v1.p.5", true),
            ("solve-result.v1.p.>", "solve-result.v1.q.5", false),
        ];
        for (filter, subject, expected) in cases {
            assert_eq!(
                subject_matches(filter, subject),
                expected,
                "subject_matches({filter:?}, {subject:?})"
            );
        }
    }

    #[test]
    fn subject_group_strips_final_token() {
        assert_eq!(subject_group("solve-result.v1.p.5"), "solve-result.v1.p");
        assert_eq!(
            subject_group("events.matched.v1.p.7"),
            "events.matched.v1.p"
        );
        assert_eq!(subject_group("single"), "single");
    }

    #[tokio::test]
    async fn publish_consume_round_trip() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut source = bus.source::<TestMsg>("s");

        let out = publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(1))
            .await
            .expect("publish");
        assert_eq!(
            out,
            PublishOutcome::Acked {
                sequence: 1,
                duplicate: false
            }
        );

        let delivery = source.next().await.expect("some").expect("ok");
        assert_eq!(delivery.item, msg(1));
        assert_eq!(delivery.subject, "s");
        assert_eq!(delivery.msg_id.as_deref(), Some("id-1"));
        assert!(!delivery.redelivered);
        assert_eq!(delivery.handle.sequence(), 1);
        assert_eq!(delivery.handle.deliveries(), 1);

        delivery.handle.ack().await.expect("ack");
        assert_eq!(bus.acked_count("s"), 1);
    }

    #[tokio::test]
    async fn sent_at_is_read_from_the_header() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut source = bus.source::<TestMsg>("s");

        let mut headers = HeaderMap::new();
        headers.insert(SENT_AT_MS, "1500");
        publisher
            .publish("s", "id-1", headers, &msg(1))
            .await
            .expect("publish");

        let delivery = source.next().await.expect("some").expect("ok");
        assert_eq!(
            delivery.sent_at,
            Some(UNIX_EPOCH + Duration::from_millis(1500))
        );
    }

    #[tokio::test]
    async fn malformed_payload_is_acked_and_skipped_without_redelivery() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut source = bus.source::<TestMsg>("s");

        publisher
            .publish_bytes("s", "poison", HeaderMap::new(), b"not postcard")
            .await
            .expect("publish poison bytes");
        publisher
            .publish("s", "good", HeaderMap::new(), &msg(7))
            .await
            .expect("publish good message");

        // The caller never receives a handle for poison, because the adapter
        // has already retired it; the next value is the useful message.
        let good = source
            .next()
            .await
            .expect("good delivery")
            .expect("decoded");
        assert_eq!(good.item, msg(7));
        assert_eq!(bus.acked_count("s"), 1, "the poison was acknowledged");

        // An ack-wait sweep must not resurrect a poison record the caller
        // could not acknowledge itself.
        bus.redeliver_unacked("s");
        good.handle.ack().await.expect("ack good message");
        assert_eq!(bus.acked_count("s"), 2);
    }

    #[tokio::test]
    async fn duplicate_msg_id_is_reported_and_not_redelivered() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut source = bus.source::<TestMsg>("s");

        let first = publisher
            .publish("s", "dup", HeaderMap::new(), &msg(1))
            .await
            .expect("publish");
        let second = publisher
            .publish("s", "dup", HeaderMap::new(), &msg(2))
            .await
            .expect("publish");

        assert_eq!(
            first,
            PublishOutcome::Acked {
                sequence: 1,
                duplicate: false
            }
        );
        assert_eq!(
            second,
            PublishOutcome::Acked {
                sequence: 1,
                duplicate: true
            }
        );
        assert_eq!(bus.published("s").len(), 1);

        let delivery = source.next().await.expect("some").expect("ok");
        assert_eq!(delivery.item, msg(1));
    }

    #[tokio::test]
    async fn concurrent_duplicate_publishes_store_one_message() {
        let bus = MemoryBus::new();
        let mut publishers = tokio::task::JoinSet::new();

        for n in 0..32 {
            let publisher = bus.publisher::<TestMsg>();
            publishers.spawn(async move {
                publisher
                    .publish("s", "same-id", HeaderMap::new(), &msg(n))
                    .await
                    .expect("publish")
            });
        }

        let mut fresh = 0;
        let mut duplicates = 0;
        while let Some(outcome) = publishers.join_next().await {
            match outcome.expect("publisher task joins") {
                PublishOutcome::Acked {
                    sequence: 1,
                    duplicate: false,
                } => fresh += 1,
                PublishOutcome::Acked {
                    sequence: 1,
                    duplicate: true,
                } => duplicates += 1,
                outcome => panic!("unexpected concurrent publish outcome: {outcome:?}"),
            }
        }

        assert_eq!(fresh, 1, "one publish wins the stream sequence");
        assert_eq!(duplicates, 31, "every concurrent retry deduplicates");
        assert_eq!(bus.published("s").len(), 1);
    }

    #[tokio::test]
    async fn two_consumers_each_see_every_message() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut first = bus.source::<TestMsg>("s");
        let mut second = bus.source::<TestMsg>("s");

        for n in 1..=2 {
            publisher
                .publish("s", &format!("id-{n}"), HeaderMap::new(), &msg(n))
                .await
                .expect("publish");
        }

        assert_eq!(first.next().await.unwrap().unwrap().item, msg(1));
        assert_eq!(first.next().await.unwrap().unwrap().item, msg(2));
        assert_eq!(second.next().await.unwrap().unwrap().item, msg(1));
        assert_eq!(second.next().await.unwrap().unwrap().item, msg(2));
    }

    #[tokio::test(start_paused = true)]
    async fn nak_redelivers_to_the_same_consumer() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut source = bus.source::<TestMsg>("s");

        publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(1))
            .await
            .expect("publish");

        let first = source.next().await.unwrap().unwrap();
        assert_eq!(first.handle.deliveries(), 1);
        assert!(!first.redelivered);
        first
            .handle
            .nak(Some(Duration::from_millis(250)))
            .await
            .expect("nak");

        tokio::time::advance(Duration::from_millis(250)).await;
        let again = source.next().await.unwrap().unwrap();
        assert!(again.redelivered);
        assert_eq!(again.handle.deliveries(), 2);
        assert_eq!(again.handle.sequence(), 1);
        assert_eq!(bus.nak_delays(), vec![Some(Duration::from_millis(250))]);
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_nak_does_not_resurrect_a_sequence_acked_before_the_timer() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut source = bus.source::<TestMsg>("s");

        publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(1))
            .await
            .expect("publish");
        source
            .next()
            .await
            .unwrap()
            .unwrap()
            .handle
            .nak(Some(Duration::from_millis(250)))
            .await
            .expect("nak");

        // Model another ack-wait copy reaching the same consumer before the
        // delayed NAK timer fires.
        bus.redeliver_unacked("s");
        source
            .next()
            .await
            .unwrap()
            .unwrap()
            .handle
            .ack()
            .await
            .expect("ack copy");
        tokio::time::advance(Duration::from_millis(250)).await;
        tokio::task::yield_now().await;

        let inner = bus.0.lock().unwrap();
        let consumer = inner.consumers.get(&source.consumer_id).unwrap();
        assert!(consumer.acked.contains(&1));
        assert!(
            consumer.redeliver.is_empty(),
            "acked sequence stayed retired"
        );
    }

    #[tokio::test]
    async fn ambiguous_publish_stores_and_retry_dedups() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();

        bus.fail_next_publish(PublishError::Ambiguous(anyhow::anyhow!("timeout")));
        let err = publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(1))
            .await
            .expect_err("ambiguous");
        assert!(err.is_ambiguous());

        let retry = publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(1))
            .await
            .expect("retry");
        assert!(matches!(
            retry,
            PublishOutcome::Acked {
                duplicate: true,
                ..
            }
        ));
        assert_eq!(bus.published("s").len(), 1);
    }

    #[tokio::test]
    async fn failed_publish_stores_nothing() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();

        bus.fail_next_publish(PublishError::Failed(anyhow::anyhow!("refused")));
        let err = publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(1))
            .await
            .expect_err("failed");
        assert!(!err.is_ambiguous());
        assert!(bus.published("s").is_empty());

        publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(1))
            .await
            .expect("publish");
        assert_eq!(bus.published("s").len(), 1);
    }

    #[tokio::test]
    async fn redeliver_unacked_only_touches_unacked() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut source = bus.source::<TestMsg>("s");

        for n in 1..=2 {
            publisher
                .publish("s", &format!("id-{n}"), HeaderMap::new(), &msg(n))
                .await
                .expect("publish");
        }

        let first = source.next().await.unwrap().unwrap();
        let second = source.next().await.unwrap().unwrap();
        let second_seq = second.handle.sequence();
        first.handle.ack().await.expect("ack");
        // `second` is left unacked.

        bus.redeliver_unacked("s");

        let redelivered = source.next().await.unwrap().unwrap();
        assert!(redelivered.redelivered);
        assert_eq!(redelivered.item, msg(2));
        assert_eq!(redelivered.handle.sequence(), second_seq);
        assert_eq!(redelivered.handle.deliveries(), 2);
    }

    #[tokio::test]
    async fn source_next_wakes_on_publish() {
        let bus = MemoryBus::new();
        let mut source = bus.source::<TestMsg>("s");

        let producer = bus.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            producer
                .publisher::<TestMsg>()
                .publish("s", "id-1", HeaderMap::new(), &msg(7))
                .await
                .expect("publish");
        });

        let delivery = source.next().await.expect("some").expect("ok");
        assert_eq!(delivery.item, msg(7));
        task.await.expect("producer joins");
    }

    #[tokio::test]
    async fn source_next_returns_none_after_close() {
        let bus = MemoryBus::new();
        let mut source = bus.source::<TestMsg>("s");
        bus.close();
        assert!(source.next().await.is_none());
    }

    #[tokio::test]
    async fn consumer_fetch_is_capacity_bounded() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut consumer = bus.consumer::<TestMsg>("s.>");

        for n in 0..5 {
            publisher
                .publish(
                    &format!("s.{n}"),
                    &format!("id-{n}"),
                    HeaderMap::new(),
                    &msg(n),
                )
                .await
                .expect("publish");
        }

        let first = consumer
            .fetch(3, Duration::from_millis(50))
            .await
            .expect("fetch");
        assert_eq!(first.len(), 3);
        let second = consumer
            .fetch(10, Duration::from_millis(50))
            .await
            .expect("fetch");
        assert_eq!(second.len(), 2);

        let empty = consumer
            .fetch(1, Duration::from_millis(10))
            .await
            .expect("fetch");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn consumer_fetch_wakes_on_publish() {
        let bus = MemoryBus::new();
        let mut consumer = bus.consumer::<TestMsg>("s");

        let producer = bus.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            producer
                .publisher::<TestMsg>()
                .publish("s", "id-1", HeaderMap::new(), &msg(3))
                .await
                .expect("publish");
        });

        let batch = consumer
            .fetch(4, Duration::from_secs(1))
            .await
            .expect("fetch");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].item, msg(3));
        task.await.expect("producer joins");
    }

    #[tokio::test]
    async fn clear_resets_log_and_progress() {
        let bus = MemoryBus::new();
        let publisher = bus.publisher::<TestMsg>();
        let mut source = bus.source::<TestMsg>("s");

        publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(1))
            .await
            .expect("publish");
        source.next().await.unwrap().unwrap();

        bus.clear();
        assert!(bus.published("s").is_empty());

        let out = publisher
            .publish("s", "id-1", HeaderMap::new(), &msg(9))
            .await
            .expect("publish");
        assert_eq!(
            out,
            PublishOutcome::Acked {
                sequence: 1,
                duplicate: false
            }
        );
        assert_eq!(source.next().await.unwrap().unwrap().item, msg(9));
    }
}

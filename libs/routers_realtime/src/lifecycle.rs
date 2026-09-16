//! Shutdown and drain coordination shared by every binary.
//!
//! [`Shutdown`] stops intake; [`Drain`] tracks in-flight work and waits for it
//! to finish or a grace budget to elapse; [`Readiness`] is the orthogonal
//! outward serving signal. A commit that was prepared but not yet published is
//! left in the store for recovery, never rolled back here.

use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use core::time::Duration;
use std::path::PathBuf;

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Why a drain was started; bounded so it can double as a metric label.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DrainReason {
    /// A process signal (SIGTERM/SIGINT) asked us to stop.
    Signal = 1,
    /// A readiness probe or dependency reported us unfit to serve.
    Unready = 2,
    /// A human or control plane requested the drain.
    Operator = 3,
    /// An unrecoverable error forces the process down.
    Fatal = 4,
}

impl DrainReason {
    /// Bounded, lowercase label suitable for logs and metric dimensions.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Signal => "signal",
            Self::Unready => "unready",
            Self::Operator => "operator",
            Self::Fatal => "fatal",
        }
    }

    /// Decode the atomic encoding used by [`Shutdown`]; `0` maps to `None`.
    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Signal),
            2 => Some(Self::Unready),
            3 => Some(Self::Operator),
            4 => Some(Self::Fatal),
            _ => None,
        }
    }
}

impl fmt::Display for DrainReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_label())
    }
}

/// Cooperative shutdown signal shared across a binary's tasks. Carries the
/// first [`DrainReason`]; every clone observes the same trigger.
#[derive(Clone, Debug)]
pub struct Shutdown {
    token: CancellationToken,
    // First-writer-wins: `0` means no reason recorded yet.
    reason: Arc<AtomicU8>,
}

impl Shutdown {
    /// A fresh, untriggered handle.
    #[must_use]
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Arc::new(AtomicU8::new(0)),
        }
    }

    /// A child handle: cancelled when this one is triggered, but able to be
    /// triggered independently. The recorded reason is shared both ways.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            token: self.token.child_token(),
            reason: Arc::clone(&self.reason),
        }
    }

    /// Start the drain. Idempotent: the first call wins and records `reason`;
    /// later calls (with any reason) only ensure the token is cancelled.
    pub fn trigger(&self, reason: DrainReason) {
        // Record the reason before cancelling so observers see both.
        let _ = self
            .reason
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
        self.token.cancel();
    }

    /// Whether the drain has started for this handle (or its parent).
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        self.token.is_cancelled()
    }

    /// The reason recorded by the first [`Shutdown::trigger`], once triggered.
    #[must_use]
    pub fn reason(&self) -> Option<DrainReason> {
        if self.token.is_cancelled() {
            DrainReason::from_u8(self.reason.load(Ordering::Acquire))
        } else {
            None
        }
    }

    /// Resolves as soon as the drain is triggered; returns immediately if it
    /// already has.
    pub async fn triggered(&self) {
        self.token.cancelled().await;
    }

    /// Spawn a background task that triggers this handle on SIGTERM or SIGINT
    /// and return the handle. Must be called from within a Tokio runtime.
    #[must_use]
    pub fn from_signals() -> Self {
        let shutdown = Self::new();
        let handle = shutdown.clone();
        tokio::spawn(async move {
            if wait_for_signal().await {
                handle.trigger(DrainReason::Signal);
            }
        });
        shutdown
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// Await the first shutdown signal, resolving to `true` once one is observed.
#[cfg(unix)]
async fn wait_for_signal() -> bool {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    true
}

#[cfg(not(unix))]
async fn wait_for_signal() -> bool {
    tokio::signal::ctrl_c().await.is_ok()
}

/// Cooperative in-flight tracking for the drain phase. Each unit of work holds
/// an [`InFlight`] guard; cloning shares the counter and [`Drain::quiesce`]
/// parks rather than polls.
#[derive(Clone, Default)]
pub struct Drain {
    inner: Arc<DrainInner>,
}

#[derive(Default)]
struct DrainInner {
    count: AtomicUsize,
    idle: Notify,
}

impl Drain {
    /// A fresh tracker with nothing in flight.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one unit of in-flight work. The count decrements when the
    /// returned guard is dropped.
    #[must_use = "dropping the guard immediately ends the work it should track"]
    pub fn begin(&self) -> InFlight {
        self.inner.count.fetch_add(1, Ordering::AcqRel);
        InFlight {
            inner: Arc::clone(&self.inner),
        }
    }

    /// How many [`InFlight`] guards are currently alive.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.inner.count.load(Ordering::Acquire)
    }

    /// Wait until nothing is in flight, or until `grace` elapses. Returns
    /// [`QuiesceOutcome::Drained`] the moment the count reaches zero, or
    /// [`QuiesceOutcome::TimedOut`] carrying the still-outstanding count.
    pub async fn quiesce(&self, grace: Duration) -> QuiesceOutcome {
        let deadline = Instant::now() + grace;
        loop {
            if self.outstanding() == 0 {
                return QuiesceOutcome::Drained;
            }
            // Arm the notification before re-checking so a concurrent drop is not missed.
            let idle = self.inner.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.outstanding() == 0 {
                return QuiesceOutcome::Drained;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return QuiesceOutcome::TimedOut {
                    remaining: self.outstanding(),
                };
            }
            if tokio::time::timeout(remaining, idle).await.is_err() {
                return QuiesceOutcome::TimedOut {
                    remaining: self.outstanding(),
                };
            }
        }
    }
}

/// RAII guard for one unit of in-flight work; decrements its [`Drain`] on drop.
#[must_use = "the guard must live for the duration of the work it tracks"]
pub struct InFlight {
    inner: Arc<DrainInner>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        // Only the 1->0 transition can unblock a `quiesce`.
        if self.inner.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.idle.notify_waiters();
        }
    }
}

/// Result of [`Drain::quiesce`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum QuiesceOutcome {
    /// Everything finished within the grace budget.
    Drained,
    /// The budget elapsed with `remaining` guards still in flight.
    TimedOut { remaining: usize },
}

/// Namespace for constructing a readiness `watch` pair.
pub struct Readiness;

impl Readiness {
    /// Create a linked [`ReadinessSetter`]/[`ReadinessWatcher`] pair starting in
    /// [`ReadyState::Starting`].
    // Named `new` per the shared design contract; returns the pair, not `Self`.
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn new() -> (ReadinessSetter, ReadinessWatcher) {
        let (tx, rx) = watch::channel(ReadyState::Starting);
        (
            ReadinessSetter {
                tx,
                ready_file: None,
            },
            ReadinessWatcher { rx },
        )
    }
}

/// Outward-facing serving state, published to every [`ReadinessWatcher`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReadyState {
    /// Booting: loading graphs, connecting to the broker, not yet serving.
    Starting,
    /// Serving normally.
    Ready,
    /// Draining: intake stopped, finishing in-flight work.
    Draining,
    /// Failed and unable to serve.
    Failed,
}

impl fmt::Display for ReadyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::Failed => "failed",
        };
        f.write_str(label)
    }
}

/// The write side of a readiness channel. When a ready file is attached via
/// [`ReadinessSetter::with_ready_file`], [`ReadyState::Ready`] creates it and
/// any other state removes it; file maintenance is best-effort.
pub struct ReadinessSetter {
    tx: watch::Sender<ReadyState>,
    ready_file: Option<PathBuf>,
}

impl ReadinessSetter {
    /// Attach a marker file mirrored to the [`ReadyState::Ready`] state.
    #[must_use]
    pub fn with_ready_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.ready_file = Some(path.into());
        self
    }

    /// Publish a new state to all watchers and reconcile the ready file. Always
    /// notifies, even if unchanged, so a re-assertion still wakes a `changed()`.
    pub fn set(&self, state: ReadyState) {
        self.tx.send_replace(state);
        self.sync_ready_file(state);
    }

    fn sync_ready_file(&self, state: ReadyState) {
        let Some(path) = self.ready_file.as_ref() else {
            return;
        };
        match state {
            ReadyState::Ready => {
                if let Err(err) = std::fs::File::create(path) {
                    warn!(path = %path.display(), error = %err, "failed to create readiness file");
                }
            }
            _ => {
                // An already-absent file is the desired end state, not a failure.
                if path.exists()
                    && let Err(err) = std::fs::remove_file(path)
                {
                    warn!(path = %path.display(), error = %err, "failed to remove readiness file");
                }
            }
        }
    }
}

/// The read side of a readiness channel; cheap to clone.
#[derive(Clone)]
pub struct ReadinessWatcher {
    rx: watch::Receiver<ReadyState>,
}

impl ReadinessWatcher {
    /// The most recently published state.
    #[must_use]
    pub fn current(&self) -> ReadyState {
        *self.rx.borrow()
    }

    /// Await the next transition and return the new state. Once every setter is
    /// dropped, returns the last known state immediately.
    pub async fn changed(&mut self) -> ReadyState {
        let _ = self.rx.changed().await;
        *self.rx.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn drain_reason_labels_are_stable() {
        assert_eq!(DrainReason::Signal.to_string(), "signal");
        assert_eq!(DrainReason::Unready.to_string(), "unready");
        assert_eq!(DrainReason::Operator.to_string(), "operator");
        assert_eq!(DrainReason::Fatal.to_string(), "fatal");
    }

    #[tokio::test]
    async fn trigger_is_idempotent_and_keeps_first_reason() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.is_triggered());
        assert_eq!(shutdown.reason(), None);

        shutdown.trigger(DrainReason::Operator);
        shutdown.trigger(DrainReason::Fatal);

        assert!(shutdown.is_triggered());
        assert_eq!(shutdown.reason(), Some(DrainReason::Operator));

        shutdown.triggered().await;
    }

    #[tokio::test]
    async fn child_sees_parent_trigger() {
        let parent = Shutdown::new();
        let child = parent.child();
        assert!(!child.is_triggered());
        assert_eq!(child.reason(), None);

        parent.trigger(DrainReason::Signal);

        assert!(child.is_triggered());
        assert_eq!(child.reason(), Some(DrainReason::Signal));
        child.triggered().await;
    }

    #[tokio::test]
    async fn child_trigger_does_not_cancel_parent() {
        let parent = Shutdown::new();
        let child = parent.child();

        child.trigger(DrainReason::Unready);

        assert!(child.is_triggered());
        assert!(!parent.is_triggered());
        // Untriggered parent reports no reason even though the store is shared.
        assert_eq!(parent.reason(), None);
    }

    #[tokio::test]
    async fn quiesce_returns_immediately_when_idle() {
        let drain = Drain::new();
        assert_eq!(drain.outstanding(), 0);
        assert_eq!(
            drain.quiesce(Duration::from_secs(5)).await,
            QuiesceOutcome::Drained
        );
    }

    #[tokio::test]
    async fn quiesce_drains_when_guards_drop() {
        let drain = Drain::new();
        let first = drain.begin();
        let second = drain.begin();
        assert_eq!(drain.outstanding(), 2);

        let worker = drain.clone();
        let task = tokio::spawn(async move {
            // Staggered so quiesce sees the 2->1->0 walk.
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(first);
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(second);
            drop(worker);
        });

        assert_eq!(
            drain.quiesce(Duration::from_secs(5)).await,
            QuiesceOutcome::Drained
        );
        assert_eq!(drain.outstanding(), 0);
        task.await.expect("worker task joins");
    }

    #[tokio::test]
    async fn quiesce_times_out_with_outstanding_work() {
        let drain = Drain::new();
        let _guard = drain.begin();
        let _other = drain.begin();

        assert_eq!(
            drain.quiesce(Duration::from_millis(20)).await,
            QuiesceOutcome::TimedOut { remaining: 2 }
        );
        assert_eq!(drain.outstanding(), 2);
    }

    #[tokio::test]
    async fn readiness_watcher_observes_transitions() {
        let (setter, mut watcher) = Readiness::new();
        assert_eq!(watcher.current(), ReadyState::Starting);

        setter.set(ReadyState::Ready);
        assert_eq!(watcher.changed().await, ReadyState::Ready);
        assert_eq!(watcher.current(), ReadyState::Ready);

        setter.set(ReadyState::Draining);
        assert_eq!(watcher.changed().await, ReadyState::Draining);
    }

    fn unique_temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "routers-realtime-{tag}-{}-{n}.flag",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn ready_file_appears_and_disappears() {
        let path = unique_temp_path("ready");
        let _ = std::fs::remove_file(&path);

        let (setter, _watcher) = Readiness::new();
        let setter = setter.with_ready_file(&path);
        assert!(!path.exists());

        setter.set(ReadyState::Ready);
        assert!(path.exists(), "Ready should create the marker file");

        setter.set(ReadyState::Draining);
        assert!(!path.exists(), "Draining should remove the marker file");

        setter.set(ReadyState::Ready);
        assert!(path.exists());

        setter.set(ReadyState::Failed);
        assert!(!path.exists());

        let _ = std::fs::remove_file(&path);
    }
}

//! Shutdown and drain coordination shared by every binary. (T22)
//!
//! A Kubernetes readiness flag does not stop a NATS pull loop — it only steers
//! traffic at the edge. Stopping a process cleanly is a three-step dance that
//! this module makes explicit so every binary performs it the same way:
//!
//! 1. **Stop intake first.** Something calls [`Shutdown::trigger`] (a signal, an
//!    operator, a failed readiness probe). Every intake loop `select!`s over
//!    [`Shutdown::triggered`] and, once it resolves, stops pulling new work.
//! 2. **Finish or abandon in-flight work second.** Work already claimed is
//!    tracked with [`Drain`]: each unit of work holds an [`InFlight`] guard for
//!    its lifetime. [`Drain::quiesce`] waits — without busy-looping — until the
//!    outstanding count reaches zero or a grace budget elapses.
//! 3. **Exit third.** The binary drops its resources and returns. A commit that
//!    was *prepared* but not yet *published* is deliberately left in the store
//!    for recovery to finish; it is never rolled back here, because rolling back
//!    a durable prepare would lose the acknowledgement that made it durable.
//!
//! [`Readiness`] is the orthogonal outward signal: a `watch` channel whose state
//! a health endpoint (or an exec probe via [`ReadinessSetter::with_ready_file`])
//! reflects. Triggering shutdown flips readiness to [`ReadyState::Draining`];
//! the two are wired together by the binary, not by this module, so tests can
//! exercise each in isolation.

use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use core::time::Duration;
use std::path::PathBuf;

use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Why a drain was started. A small, bounded set so it can double as a metric
/// label without unbounded cardinality.
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

    /// Decode the atomic encoding used by [`Shutdown`]. `0` is the "unset"
    /// sentinel and maps to `None`.
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

/// Cooperative shutdown signal shared across a binary's tasks.
///
/// Wraps a [`CancellationToken`] so intake loops can `select!` on
/// [`Shutdown::triggered`], and carries the *first* [`DrainReason`] so the exit
/// path can log and label why it is stopping. Cloning is cheap and every clone
/// observes the same trigger; [`Shutdown::child`] hands out a scoped token that
/// still sees the parent's trigger but can be cancelled on its own.
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
    /// triggered independently without disturbing the parent. The recorded
    /// reason is shared, so a child observes the parent's reason and vice
    /// versa.
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
        // Record the reason before cancelling so any observer that sees the
        // cancellation also sees a reason.
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
    /// already has. Intended for the `select!` arm of an intake loop.
    pub async fn triggered(&self) {
        self.token.cancelled().await;
    }

    /// Spawn a background task that triggers this handle on SIGTERM or SIGINT
    /// and return the handle. Must be called from within a Tokio runtime.
    ///
    /// On unix both `SIGTERM` and `SIGINT` are watched; elsewhere `ctrl_c` is
    /// the fallback.
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
///
/// On unix both `SIGTERM` and `SIGINT` are watched; elsewhere `ctrl_c` is the
/// fallback. A missing handler means the process cannot honour a graceful
/// stop, so failing loudly beats silently ignoring `SIGTERM`.
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

/// Cooperative in-flight tracking for the drain phase.
///
/// Each unit of work calls [`Drain::begin`] and holds the returned [`InFlight`]
/// guard for its whole lifetime; the count drops when the guard does. Cloning a
/// `Drain` shares the same counter, so producer and drainer can hold separate
/// handles. Backed by an atomic counter plus a [`Notify`], so [`Drain::quiesce`]
/// sleeps rather than polls.
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

    /// Wait until nothing is in flight, or until `grace` elapses.
    ///
    /// Returns [`QuiesceOutcome::Drained`] the moment the count reaches zero, or
    /// [`QuiesceOutcome::TimedOut`] carrying the still-outstanding count when the
    /// budget runs out. Never busy-loops: it parks on the internal [`Notify`]
    /// that the last guard's drop wakes.
    pub async fn quiesce(&self, grace: Duration) -> QuiesceOutcome {
        let deadline = Instant::now() + grace;
        loop {
            if self.outstanding() == 0 {
                return QuiesceOutcome::Drained;
            }
            // Arm the notification *before* re-checking so a drop that lands
            // between the check and the await is not missed.
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
        // Only the transition to zero can unblock a `quiesce`, so only then do
        // we wake waiters.
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
    /// The budget elapsed with `remaining` guards still in flight; the caller
    /// decides whether to wait longer or abandon them to recovery.
    TimedOut { remaining: usize },
}

/// Namespace for constructing a readiness `watch` pair.
pub struct Readiness;

impl Readiness {
    /// Create a linked [`ReadinessSetter`]/[`ReadinessWatcher`] pair starting in
    /// [`ReadyState::Starting`].
    // Named `new` per the shared design contract; it deliberately returns the
    // setter/watcher pair rather than `Self`.
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

/// The write side of a readiness channel.
///
/// [`ReadinessSetter::set`] publishes a new [`ReadyState`] to every watcher. When
/// a ready file is attached via [`ReadinessSetter::with_ready_file`], reaching
/// [`ReadyState::Ready`] creates the file and any other state removes it, so an
/// exec probe can `test -f <path>`. File maintenance is best-effort: failures
/// are logged, never propagated.
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

    /// Publish a new state to all watchers and reconcile the ready file.
    ///
    /// Always notifies watchers, even if the state is unchanged, so a
    /// re-assertion still wakes a `changed()` await.
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
                // Removing an already-absent file is the desired end state, not
                // a failure worth logging; only a present-but-unremovable file
                // is surfaced.
                if path.exists()
                    && let Err(err) = std::fs::remove_file(path)
                {
                    warn!(path = %path.display(), error = %err, "failed to remove readiness file");
                }
            }
        }
    }
}

/// The read side of a readiness channel. Cheap to clone; every clone observes
/// the same transitions.
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

    /// Await the next transition and return the new state. If every setter has
    /// been dropped there will be no further transitions, so the last known
    /// state is returned immediately.
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
        // A second trigger with a different reason must not overwrite the first.
        shutdown.trigger(DrainReason::Fatal);

        assert!(shutdown.is_triggered());
        assert_eq!(shutdown.reason(), Some(DrainReason::Operator));

        // `triggered()` resolves immediately once triggered.
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
        // Gated on cancellation, so the untriggered parent reports no reason
        // even though the reason store is shared.
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
            // Drop staggered so `quiesce` sees the 2 -> 1 -> 0 walk and only
            // returns on the final transition.
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(first);
            tokio::time::sleep(Duration::from_millis(10)).await;
            drop(second);
            // Keep the clone alive until here.
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
        // The guards are still alive, so the count is unchanged.
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

        // Re-asserting Ready recreates it; removing an absent file is fine.
        setter.set(ReadyState::Ready);
        assert!(path.exists());

        setter.set(ReadyState::Failed);
        assert!(!path.exists());

        let _ = std::fs::remove_file(&path);
    }
}

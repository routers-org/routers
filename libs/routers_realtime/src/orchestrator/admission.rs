//! Orchestrator admission control (spec §3).
//!
//! Every partition worker in a process shares one [`Admission`] controller. It
//! is the single place where the orchestrator decides whether a new solve job
//! may be dispatched, capping the number of *outstanding* jobs and the bytes
//! they carry, both globally for the process and per region. Work that cannot
//! be admitted is left waiting *outside* the matcher CPU queues — a job is only
//! published once a [`Permit`] has been reserved for it — so backpressure lands
//! here rather than piling unbounded work onto the solve plane.
//!
//! # Why credits, not a semaphore
//!
//! A plain counting semaphore would cap job *count* but not the bytes a job
//! carries, and a large trip can be two orders of magnitude bigger than a
//! small one. Admission therefore reserves against two independent ceilings
//! per scope — a job count and a byte budget — and a request must fit *both*.
//! The controller is lock-free: each scope is a small block of atomics, so the
//! hot dispatch path never blocks another partition worker.
//!
//! # Reservation order and rollback
//!
//! A request reserves the **region** scope first and the **global** scope
//! second. If the global scope is full the region reservation is rolled back,
//! so a request never leaves a scope charged for a job it did not get. The
//! returned [`Permit`] owns the reservation and releases both scopes when it is
//! dropped (RAII), which is what makes "outstanding" mean "a live permit
//! exists" without any bookkeeping in the worker.
//!
//! # Oversized jobs
//!
//! A single job whose bytes exceed a scope's whole byte budget can never fit
//! alongside anything else, so a naive check would make it wait forever. The
//! controller admits such a lone job when its scope is otherwise idle (holds no
//! other job), trading a momentary budget overshoot for liveness. See
//! [`Credits::try_reserve`].
//!
//! # Held demand
//!
//! Demand that the scheduler is holding *before* dispatch — work it wants to
//! run but cannot yet admit — is counted separately via [`Admission::hold`],
//! which hands back a [`Waiting`] RAII guard. That count never gates admission;
//! it exists only so metrics (T30) and logs can show queued pressure that the
//! job/byte gauges alone would hide.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;

use thiserror::Error;

use crate::metrics::{AdmissionGauges, AdmissionRow, Metrics};
use crate::protocol::ids::RegionId;

/// Configured credit ceilings for the admission controller.
///
/// The four scalar fields set the process-wide (`global_*`) and per-region
/// default (`region_*`) ceilings; `overrides` raises or lowers the ceilings for
/// named regions (a busy metropolitan region may warrant more headroom than a
/// rural one). All limits are static configuration — the controller never calls
/// an autoscaler, it only accounts for what is already outstanding locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionConfig {
    /// Most jobs that may be outstanding across the whole process.
    pub global_jobs: u64,
    /// Most job bytes that may be outstanding across the whole process.
    pub global_bytes: u64,
    /// Default per-region ceiling on outstanding jobs.
    pub region_jobs: u64,
    /// Default per-region ceiling on outstanding job bytes.
    pub region_bytes: u64,
    /// Per-region ceiling overrides, keyed by region id: `(jobs, bytes)`. A
    /// region absent from this map uses `region_jobs` / `region_bytes`.
    pub overrides: HashMap<RegionId, (u64, u64)>,
}

impl Default for AdmissionConfig {
    /// Ceilings sized for a single orchestrator process: 4096 outstanding jobs
    /// and 512 MiB globally, 1024 jobs and 128 MiB per region. The per-region
    /// defaults deliberately sum to more than the global ceiling across several
    /// regions, so the global scope is the real backstop and a single quiet
    /// region cannot be starved by its own conservative default.
    fn default() -> Self {
        Self {
            global_jobs: 4096,
            global_bytes: 512 * 1024 * 1024,
            region_jobs: 1024,
            region_bytes: 128 * 1024 * 1024,
            overrides: HashMap::new(),
        }
    }
}

/// One credit scope: the live tallies and the ceilings they are checked
/// against. A scope is either the process-wide global scope (`region` is
/// `None`) or one region's scope (`region` is `Some`). The `jobs`/`bytes`
/// atomics are the reserved-but-not-yet-released totals; `waiting` is the
/// held-demand gauge, which never gates admission.
///
/// Memory ordering is [`Ordering::Relaxed`] throughout: the counters guard no
/// other shared memory, so only their own atomicity matters, and a
/// compare-and-swap keeps every reservation from ever exceeding a ceiling
/// regardless of ordering.
#[derive(Debug)]
struct Credits {
    /// The region this scope accounts for, or `None` for the global scope.
    region: Option<RegionId>,
    /// Outstanding job count (reserved, not yet released).
    jobs: AtomicU64,
    /// Outstanding job bytes (reserved, not yet released).
    bytes: AtomicU64,
    /// Held demand: work waiting to be admitted. Reported, never gating.
    waiting: AtomicU64,
    /// Ceiling on `jobs`.
    max_jobs: u64,
    /// Ceiling on `bytes`.
    max_bytes: u64,
}

/// Which ceiling of a scope blocked a reservation. Mapped to a scope-qualified
/// [`HeldReason`] by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CapKind {
    /// The job-count ceiling was reached.
    Jobs,
    /// The byte-budget ceiling was reached.
    Bytes,
}

impl Credits {
    fn new(region: Option<RegionId>, max_jobs: u64, max_bytes: u64) -> Self {
        Self {
            region,
            jobs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            waiting: AtomicU64::new(0),
            max_jobs,
            max_bytes,
        }
    }

    /// Reserve one job of `bytes` bytes against this scope, or report which
    /// ceiling blocked it. On any `Err` the scope is left exactly as it was
    /// found; on `Ok` it holds one more job and `bytes` more bytes.
    ///
    /// The job slot is taken first so that the oversized-job exception can ask
    /// "am I the only job here?": a request whose bytes exceed `max_bytes` is
    /// admitted only when it is the sole outstanding job in the scope
    /// (`jobs == 1` after this reservation and no bytes yet reserved). That
    /// lets a job larger than the entire budget make progress when the scope is
    /// idle instead of waiting forever, while a scope that already holds work
    /// still rejects it.
    fn try_reserve(&self, bytes: u64) -> Result<(), CapKind> {
        // Reserve a job slot with a CAS loop so we never exceed `max_jobs`.
        let mut jobs = self.jobs.load(Ordering::Relaxed);
        loop {
            if jobs >= self.max_jobs {
                return Err(CapKind::Jobs);
            }
            match self.jobs.compare_exchange_weak(
                jobs,
                jobs + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => jobs = actual,
            }
        }

        // Reserve the bytes, rolling the job slot back if they do not fit.
        let mut cur = self.bytes.load(Ordering::Relaxed);
        loop {
            let would_be = cur.saturating_add(bytes);
            if would_be > self.max_bytes {
                // Over budget. Admit only the lone oversized job: this
                // reservation holds the only job slot and no bytes are yet
                // charged, so nothing else can be starved by the overshoot.
                let sole = cur == 0 && self.jobs.load(Ordering::Relaxed) == 1;
                if !sole {
                    self.jobs.fetch_sub(1, Ordering::Relaxed);
                    return Err(CapKind::Bytes);
                }
            }
            match self.bytes.compare_exchange_weak(
                cur,
                cur.saturating_add(bytes),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => cur = actual,
            }
        }
    }

    /// Release one job of `bytes` bytes previously reserved with
    /// [`try_reserve`](Self::try_reserve). Reservations and releases are always
    /// paired through [`Permit`], so the counters stay balanced.
    fn release(&self, bytes: u64) {
        self.jobs.fetch_sub(1, Ordering::Relaxed);
        self.bytes.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// A point-in-time reading of this scope for metrics and logs.
    fn usage(&self) -> RegionUsage {
        RegionUsage {
            region: self.region.clone(),
            jobs: self.jobs.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            waiting: self.waiting.load(Ordering::Relaxed),
            max_jobs: self.max_jobs,
            max_bytes: self.max_bytes,
        }
    }
}

/// The shared state behind an [`Admission`]. Built once at construction and
/// never mutated afterwards, so the map needs no lock: reservations mutate only
/// the atomics inside the `Arc<Credits>` scopes.
#[derive(Debug)]
struct Inner {
    /// The process-wide scope every request also charges against.
    global: Arc<Credits>,
    /// Per-region scopes, keyed by region id. Fixed at construction.
    regions: HashMap<RegionId, Arc<Credits>>,
}

/// The process-wide admission controller. Cheap to [`Clone`] (it is an
/// [`Arc`]) and shared by every partition worker; all methods take `&self`.
#[derive(Clone, Debug)]
pub struct Admission(Arc<Inner>);

/// Why a scope's ceiling blocked a reservation, qualified by scope so a caller
/// can tell region backpressure from global backpressure. The four variants are
/// a bounded set suitable as a metric label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldReason {
    /// The region's outstanding-job ceiling was reached.
    RegionJobs,
    /// The region's outstanding-byte ceiling was reached.
    RegionBytes,
    /// The process-wide outstanding-job ceiling was reached.
    GlobalJobs,
    /// The process-wide outstanding-byte ceiling was reached.
    GlobalBytes,
}

impl HeldReason {
    /// A stable, bounded label for this reason, for metrics and logs.
    pub fn label(self) -> &'static str {
        match self {
            HeldReason::RegionJobs => "region_jobs",
            HeldReason::RegionBytes => "region_bytes",
            HeldReason::GlobalJobs => "global_jobs",
            HeldReason::GlobalBytes => "global_bytes",
        }
    }
}

impl core::fmt::Display for HeldReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// A reservation was denied because a scope is at capacity. Carries the scope
/// and ceiling that blocked it so the caller can hold the work and report why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("admission held: {reason}")]
pub struct Held {
    /// The scope and ceiling that blocked the reservation.
    pub reason: HeldReason,
}

/// Why [`Admission::try_admit`] could not return a [`Permit`].
///
/// The task brief names both a [`Held`] outcome (a capacity ceiling was hit)
/// and an unknown-region outcome; they are unified here so `try_admit` has one
/// error type. Callers that resolved the region from the same catalog the
/// controller was built from will only ever see [`AdmitError::Held`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum AdmitError {
    /// The region id was not among those the controller was constructed for.
    #[error("unknown region")]
    UnknownRegion,
    /// A credit scope is at capacity; the work should be held and retried.
    #[error(transparent)]
    Held(#[from] Held),
}

/// A live reservation of one job's worth of credit in a region scope and the
/// global scope. Holding a `Permit` is exactly what "the job is outstanding"
/// means; dropping it releases both scopes.
///
/// A permit is deliberately **not** [`Clone`] — duplicating it would let two
/// jobs share one reservation and release it twice — and it is `Send + Sync`,
/// so it can live inside an orchestrator's `ActiveJob` across await points.
#[derive(Debug)]
pub struct Permit {
    /// The region scope this permit charges; always a named region.
    region: Arc<Credits>,
    /// The global scope this permit also charges.
    global: Arc<Credits>,
    /// The bytes reserved, released back to both scopes on drop.
    bytes: u64,
}

impl Permit {
    /// The job bytes this permit reserved.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The region this permit was admitted into.
    pub fn region(&self) -> &RegionId {
        self.region
            .region
            .as_ref()
            .expect("a permit's region scope always names a region")
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.region.release(self.bytes);
        self.global.release(self.bytes);
    }
}

/// An RAII guard counting one unit of held demand in a region scope and the
/// global scope. Created by [`Admission::hold`]; dropping it decrements the
/// `waiting` gauges. Like [`Permit`] it is not [`Clone`].
///
/// If the held region is unknown to the controller only the global gauge is
/// tracked — held demand is still demand — so the guard is always safe to
/// create.
#[derive(Debug)]
pub struct Waiting {
    /// The region scope whose `waiting` gauge was incremented, if the region
    /// was known.
    region: Option<Arc<Credits>>,
    /// The global scope whose `waiting` gauge was incremented.
    global: Arc<Credits>,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some(region) = &self.region {
            region.waiting.fetch_sub(1, Ordering::Relaxed);
        }
        self.global.waiting.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A point-in-time reading of one credit scope, for metrics (T30) and logs.
/// The same shape describes a region scope and the global scope: `region` is
/// `None` for the global aggregate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionUsage {
    /// The region this reading is for, or `None` for the global scope.
    pub region: Option<RegionId>,
    /// Outstanding jobs at the moment of the reading.
    pub jobs: u64,
    /// Outstanding job bytes at the moment of the reading.
    pub bytes: u64,
    /// Held demand at the moment of the reading.
    pub waiting: u64,
    /// The scope's job ceiling.
    pub max_jobs: u64,
    /// The scope's byte ceiling.
    pub max_bytes: u64,
}

impl Admission {
    /// Build a controller from `cfg` and the set of regions it must account
    /// for (typically `catalog.regions.iter().map(|r| &r.id)`). Each region
    /// gets its own scope, using the matching entry in `cfg.overrides` or the
    /// `region_*` defaults; a duplicate region id in the iterator keeps the
    /// last ceiling seen. Only these regions are admittable — any other id is
    /// [`AdmitError::UnknownRegion`].
    pub fn new<'a>(cfg: AdmissionConfig, regions: impl Iterator<Item = &'a RegionId>) -> Self {
        let global = Arc::new(Credits::new(None, cfg.global_jobs, cfg.global_bytes));
        let regions = regions
            .map(|id| {
                let (max_jobs, max_bytes) = cfg
                    .overrides
                    .get(id)
                    .copied()
                    .unwrap_or((cfg.region_jobs, cfg.region_bytes));
                (
                    id.clone(),
                    Arc::new(Credits::new(Some(id.clone()), max_jobs, max_bytes)),
                )
            })
            .collect();
        Admission(Arc::new(Inner { global, regions }))
    }

    /// Try to reserve one job of `bytes` bytes for `region`. Reserves the
    /// region scope, then the global scope, rolling the region reservation back
    /// if the global scope is full. On success returns a [`Permit`] that
    /// releases both scopes when dropped; on failure returns the scope and
    /// ceiling that blocked it (or [`AdmitError::UnknownRegion`]).
    pub fn try_admit(&self, region: &RegionId, bytes: u64) -> Result<Permit, AdmitError> {
        let region_credits = self
            .0
            .regions
            .get(region)
            .ok_or(AdmitError::UnknownRegion)?
            .clone();

        if let Err(kind) = region_credits.try_reserve(bytes) {
            let reason = match kind {
                CapKind::Jobs => HeldReason::RegionJobs,
                CapKind::Bytes => HeldReason::RegionBytes,
            };
            return Err(Held { reason }.into());
        }

        if let Err(kind) = self.0.global.try_reserve(bytes) {
            // The global scope is full: undo the region reservation so it is not
            // charged for a job we are not going to dispatch.
            region_credits.release(bytes);
            let reason = match kind {
                CapKind::Jobs => HeldReason::GlobalJobs,
                CapKind::Bytes => HeldReason::GlobalBytes,
            };
            return Err(Held { reason }.into());
        }

        Ok(Permit {
            region: region_credits,
            global: self.0.global.clone(),
            bytes,
        })
    }

    /// Record one unit of demand held for `region` before dispatch, returning a
    /// [`Waiting`] guard that clears it on drop. This never affects admission;
    /// it only surfaces queued pressure to metrics and logs. An unknown region
    /// counts against the global gauge only.
    pub fn hold(&self, region: &RegionId) -> Waiting {
        let region = self.0.regions.get(region).cloned();
        if let Some(credits) = &region {
            credits.waiting.fetch_add(1, Ordering::Relaxed);
        }
        self.0.global.waiting.fetch_add(1, Ordering::Relaxed);
        Waiting {
            region,
            global: self.0.global.clone(),
        }
    }

    /// A reading of every region scope, in unspecified order. Together with
    /// [`global`](Self::global) this is the whole picture the metrics layer
    /// publishes.
    pub fn snapshot(&self) -> Vec<RegionUsage> {
        self.0.regions.values().map(|c| c.usage()).collect()
    }

    /// A reading of the process-wide scope (its `region` is `None`).
    pub fn global(&self) -> RegionUsage {
        self.0.global.usage()
    }

    /// Register the admission observable gauges (`jobs_outstanding`,
    /// `admission_waiting`) against `metrics`, reading live [`snapshot`]s.
    ///
    /// The gauges are *observable*: OpenTelemetry pulls the current outstanding
    /// and waiting counts on every metrics collection through the closure
    /// installed here, so nothing on the hot dispatch path ever pushes a gauge
    /// value. The hook lives beside the controller because only it can read the
    /// credit scopes. The returned [`AdmissionGauges`] must be kept alive for
    /// the lifetime of the process (drop it to stop reporting).
    ///
    /// [`snapshot`]: Self::snapshot
    pub fn register_gauges(&self, metrics: &Metrics) -> AdmissionGauges {
        let admission = self.clone();
        metrics.register_admission(move || {
            let mut rows: Vec<AdmissionRow> = admission
                .snapshot()
                .into_iter()
                .map(|usage| AdmissionRow {
                    region: usage.region.map(|r| r.as_str().to_owned()),
                    jobs: usage.jobs,
                    waiting: usage.waiting,
                })
                .collect();
            let global = admission.global();
            rows.push(AdmissionRow {
                region: None,
                jobs: global.jobs,
                waiting: global.waiting,
            });
            rows
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(s: &str) -> RegionId {
        RegionId::new(s).unwrap()
    }

    /// A config with tiny, explicit ceilings so tests can drive scopes to their
    /// limits without huge loops.
    fn config(
        global_jobs: u64,
        global_bytes: u64,
        region_jobs: u64,
        region_bytes: u64,
    ) -> AdmissionConfig {
        AdmissionConfig {
            global_jobs,
            global_bytes,
            region_jobs,
            region_bytes,
            overrides: HashMap::new(),
        }
    }

    fn controller(cfg: AdmissionConfig, regions: &[RegionId]) -> Admission {
        Admission::new(cfg, regions.iter())
    }

    /// Assert a `try_admit` was held for exactly `reason`. Kept as a helper
    /// because `Permit` is intentionally not `PartialEq`, so the `Ok` arm
    /// cannot be compared with `assert_eq!`.
    #[track_caller]
    fn assert_held(result: Result<Permit, AdmitError>, reason: HeldReason) {
        match result {
            Err(AdmitError::Held(held)) => assert_eq!(held.reason, reason),
            other => panic!("expected held {reason:?}, got {other:?}"),
        }
    }

    #[test]
    fn defaults_match_the_spec() {
        let cfg = AdmissionConfig::default();
        assert_eq!(cfg.global_jobs, 4096);
        assert_eq!(cfg.global_bytes, 512 * 1024 * 1024);
        assert_eq!(cfg.region_jobs, 1024);
        assert_eq!(cfg.region_bytes, 128 * 1024 * 1024);
        assert!(cfg.overrides.is_empty());
    }

    #[test]
    fn unknown_region_is_rejected() {
        let a = controller(config(10, 1000, 10, 1000), &[region("known")]);
        assert_eq!(
            a.try_admit(&region("nope"), 1).unwrap_err(),
            AdmitError::UnknownRegion
        );
    }

    #[test]
    fn admit_then_release_restores_credits() {
        let r = region("r");
        let a = controller(config(10, 1000, 10, 1000), core::slice::from_ref(&r));

        {
            let permit = a.try_admit(&r, 100).expect("first admit fits");
            assert_eq!(permit.bytes(), 100);
            assert_eq!(permit.region(), &r);
            let usage = a.global();
            assert_eq!(usage.jobs, 1);
            assert_eq!(usage.bytes, 100);
        }
        // Dropping the permit returns both scopes to empty.
        assert_eq!(a.global().jobs, 0);
        assert_eq!(a.global().bytes, 0);
        assert_eq!(a.snapshot()[0].jobs, 0);
        assert_eq!(a.snapshot()[0].bytes, 0);
    }

    #[test]
    fn permit_drop_releases_incrementally() {
        let r = region("r");
        let a = controller(config(10, 1000, 10, 1000), core::slice::from_ref(&r));
        let p1 = a.try_admit(&r, 100).unwrap();
        let p2 = a.try_admit(&r, 200).unwrap();
        assert_eq!(a.global().jobs, 2);
        assert_eq!(a.global().bytes, 300);
        drop(p1);
        assert_eq!(a.global().jobs, 1);
        assert_eq!(a.global().bytes, 200);
        drop(p2);
        assert_eq!(a.global().jobs, 0);
        assert_eq!(a.global().bytes, 0);
    }

    #[test]
    fn region_job_cap_trips_before_global() {
        // Region admits 2 jobs; global would allow 100. The third is held on
        // the region job ceiling, and the global scope is never charged for it.
        let r = region("r");
        let a = controller(
            config(100, 1_000_000, 2, 1_000_000),
            core::slice::from_ref(&r),
        );
        let _p1 = a.try_admit(&r, 1).unwrap();
        let _p2 = a.try_admit(&r, 1).unwrap();
        assert_held(a.try_admit(&r, 1), HeldReason::RegionJobs);
        // Global only saw the two that were admitted.
        assert_eq!(a.global().jobs, 2);
    }

    #[test]
    fn region_byte_cap_trips() {
        let r = region("r");
        let a = controller(config(100, 1_000_000, 100, 250), core::slice::from_ref(&r));
        let _p1 = a.try_admit(&r, 200).unwrap();
        // 200 + 100 > 250, and the scope is not idle, so it is held on bytes.
        assert_held(a.try_admit(&r, 100), HeldReason::RegionBytes);
        // The held request left the scope untouched (job slot rolled back).
        assert_eq!(a.snapshot()[0].jobs, 1);
        assert_eq!(a.snapshot()[0].bytes, 200);
    }

    #[test]
    fn global_job_cap_trips_across_two_regions() {
        // Each region could take 10 jobs, but the global ceiling is 3. Filling
        // it from one region then admitting from the other trips the *global*
        // job ceiling, and the second region is rolled back to empty.
        let a = region("a");
        let b = region("b");
        let adm = controller(config(3, 1_000_000, 10, 1_000_000), &[a.clone(), b.clone()]);
        let _p1 = adm.try_admit(&a, 1).unwrap();
        let _p2 = adm.try_admit(&a, 1).unwrap();
        let _p3 = adm.try_admit(&a, 1).unwrap();
        assert_held(adm.try_admit(&b, 1), HeldReason::GlobalJobs);
        // Region b was rolled back: it holds nothing.
        let usage_b = adm
            .snapshot()
            .into_iter()
            .find(|u| u.region.as_ref() == Some(&b))
            .unwrap();
        assert_eq!(usage_b.jobs, 0);
        assert_eq!(usage_b.bytes, 0);
    }

    #[test]
    fn global_byte_cap_trips() {
        let a = region("a");
        let b = region("b");
        let adm = controller(config(100, 250, 100, 1_000_000), &[a.clone(), b.clone()]);
        let _p1 = adm.try_admit(&a, 200).unwrap();
        assert_held(adm.try_admit(&b, 100), HeldReason::GlobalBytes);
        // Region b rolled back after the global byte ceiling blocked it.
        let usage_b = adm
            .snapshot()
            .into_iter()
            .find(|u| u.region.as_ref() == Some(&b))
            .unwrap();
        assert_eq!(usage_b.jobs, 0);
        assert_eq!(usage_b.bytes, 0);
    }

    #[test]
    fn oversized_job_admitted_only_when_idle() {
        // A job larger than the whole region byte budget.
        let r = region("r");
        let a = controller(config(100, 1_000_000, 10, 100), core::slice::from_ref(&r));

        // Idle scope: the lone oversized job is admitted despite exceeding the
        // budget, so it cannot starve.
        let big = a
            .try_admit(&r, 500)
            .expect("lone oversized job is admitted");
        assert_eq!(a.snapshot()[0].bytes, 500);

        // Not idle now: a second job (even a small one that would fit on its
        // own) is held on the byte ceiling.
        assert_held(a.try_admit(&r, 10), HeldReason::RegionBytes);

        // And another oversized job is likewise held while one is outstanding.
        assert_held(a.try_admit(&r, 500), HeldReason::RegionBytes);

        // Once idle again, a normal job fits.
        drop(big);
        let _small = a.try_admit(&r, 10).expect("fits once the scope is idle");
    }

    #[test]
    fn oversized_job_denied_when_scope_holds_other_work() {
        let r = region("r");
        let a = controller(config(100, 1_000_000, 10, 100), core::slice::from_ref(&r));
        let _small = a.try_admit(&r, 10).unwrap();
        // The scope already holds a job, so the oversized one is not the sole
        // occupant and is held rather than admitted.
        assert_held(a.try_admit(&r, 500), HeldReason::RegionBytes);
    }

    #[test]
    fn oversized_global_job_admitted_when_process_idle() {
        // Region budget is generous, but the global byte budget is smaller than
        // the job. With the whole process idle the job is still admitted.
        let r = region("r");
        let a = controller(config(100, 100, 100, 1_000_000), core::slice::from_ref(&r));
        let _big = a
            .try_admit(&r, 500)
            .expect("lone oversized job clears the idle global scope");
        assert_eq!(a.global().bytes, 500);
    }

    #[test]
    fn waiting_guard_counts_and_clears() {
        let r = region("r");
        let a = controller(config(10, 1000, 10, 1000), core::slice::from_ref(&r));
        assert_eq!(a.snapshot()[0].waiting, 0);
        assert_eq!(a.global().waiting, 0);
        {
            let _w1 = a.hold(&r);
            let _w2 = a.hold(&r);
            assert_eq!(a.snapshot()[0].waiting, 2);
            assert_eq!(a.global().waiting, 2);
        }
        assert_eq!(a.snapshot()[0].waiting, 0);
        assert_eq!(a.global().waiting, 0);
    }

    #[test]
    fn hold_on_unknown_region_counts_global_only() {
        let a = controller(config(10, 1000, 10, 1000), &[region("known")]);
        {
            let _w = a.hold(&region("mystery"));
            // No region scope moved; global demand still reflects the hold.
            assert_eq!(a.global().waiting, 1);
            assert!(a.snapshot().iter().all(|u| u.waiting == 0));
        }
        assert_eq!(a.global().waiting, 0);
    }

    #[test]
    fn overrides_take_precedence_over_defaults() {
        let big = region("big");
        let small = region("small");
        let mut overrides = HashMap::new();
        overrides.insert(big.clone(), (3, 30));
        let cfg = AdmissionConfig {
            global_jobs: 100,
            global_bytes: 1_000_000,
            region_jobs: 1,
            region_bytes: 10,
            overrides,
        };
        let a = controller(cfg, &[big.clone(), small.clone()]);

        let big_usage = a
            .snapshot()
            .into_iter()
            .find(|u| u.region.as_ref() == Some(&big))
            .unwrap();
        assert_eq!(big_usage.max_jobs, 3);
        assert_eq!(big_usage.max_bytes, 30);

        let small_usage = a
            .snapshot()
            .into_iter()
            .find(|u| u.region.as_ref() == Some(&small))
            .unwrap();
        assert_eq!(small_usage.max_jobs, 1);
        assert_eq!(small_usage.max_bytes, 10);
    }

    #[test]
    fn global_usage_has_no_region() {
        let a = controller(config(10, 1000, 10, 1000), &[region("r")]);
        let g = a.global();
        assert_eq!(g.region, None);
        assert_eq!(g.max_jobs, 10);
        assert_eq!(g.max_bytes, 1000);
    }

    #[test]
    fn concurrent_admits_never_exceed_caps() {
        // Many threads hammer try_admit/drop on two shared regions. The caps
        // must never be observed exceeded, and everything must be released once
        // the storm ends.
        const THREADS: usize = 16;
        const ITERS: usize = 2_000;

        let ra = region("ra");
        let rb = region("rb");
        // Deliberately tight caps so contention actually reaches them.
        let cfg = config(12, 1_000_000, 8, 1_000_000);
        let a = controller(cfg, &[ra.clone(), rb.clone()]);

        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let a = a.clone();
                let ra = ra.clone();
                let rb = rb.clone();
                scope.spawn(move || {
                    // Split threads across the two regions.
                    let region = if t % 2 == 0 { &ra } else { &rb };
                    for _ in 0..ITERS {
                        if let Ok(permit) = a.try_admit(region, 1) {
                            // Every scope must respect its ceiling at all times.
                            let g = a.global();
                            assert!(
                                g.jobs <= g.max_jobs,
                                "global jobs {} > {}",
                                g.jobs,
                                g.max_jobs
                            );
                            for u in a.snapshot() {
                                assert!(
                                    u.jobs <= u.max_jobs,
                                    "region {:?} jobs {} > {}",
                                    u.region,
                                    u.jobs,
                                    u.max_jobs
                                );
                            }
                            drop(permit);
                        }
                    }
                });
            }
        });

        // The storm is over: no permit is outstanding anywhere.
        assert_eq!(a.global().jobs, 0);
        assert_eq!(a.global().bytes, 0);
        for u in a.snapshot() {
            assert_eq!(u.jobs, 0, "region {:?} leaked jobs", u.region);
            assert_eq!(u.bytes, 0, "region {:?} leaked bytes", u.region);
        }
    }

    #[test]
    fn concurrent_holds_balance_out() {
        // The waiting gauge is a plain counter; concurrent hold/drop must net
        // to zero.
        const THREADS: usize = 8;
        const ITERS: usize = 5_000;
        let r = region("r");
        let a = controller(config(10, 1000, 10, 1000), core::slice::from_ref(&r));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let a = a.clone();
                let r = r.clone();
                scope.spawn(move || {
                    for _ in 0..ITERS {
                        let _w = a.hold(&r);
                    }
                });
            }
        });

        assert_eq!(a.snapshot()[0].waiting, 0);
        assert_eq!(a.global().waiting, 0);
    }
}

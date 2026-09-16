//! Orchestrator admission control (spec §3).
//!
//! One process-wide [`Admission`] controller caps outstanding solve jobs by
//! count and by bytes, both globally and per region. A job is published only
//! once a [`Permit`] is reserved for it; dropping the permit releases both
//! scopes. A request reserves the region scope before the global scope and
//! rolls the region back if the global scope is full, so no scope is ever
//! charged for a job that was not admitted.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;

use thiserror::Error;

use crate::metrics::{AdmissionGauges, AdmissionRow, Metrics};
use crate::protocol::ids::RegionId;

/// Configured credit ceilings for the admission controller.
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
    /// Per-region ceiling overrides keyed by region id: `(jobs, bytes)`. Absent
    /// regions use `region_jobs` / `region_bytes`.
    pub overrides: HashMap<RegionId, (u64, u64)>,
}

impl Default for AdmissionConfig {
    /// 4096 outstanding jobs and 512 MiB globally, 1024 jobs and 128 MiB per
    /// region.
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

/// One credit scope's live tallies and ceilings; the global scope when `region`
/// is `None`, else one region's. Counters use [`Ordering::Relaxed`]: they guard
/// no other memory and a CAS keeps reservations from exceeding a ceiling.
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

/// Which ceiling of a scope blocked a reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CapKind {
    Jobs,
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
    /// ceiling blocked it. On `Err` the scope is left untouched. A lone
    /// oversized job (sole job, no bytes yet charged) is admitted despite
    /// exceeding `max_bytes`, so it makes progress instead of waiting forever.
    fn try_reserve(&self, bytes: u64) -> Result<(), CapKind> {
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

        let mut cur = self.bytes.load(Ordering::Relaxed);
        loop {
            let would_be = cur.saturating_add(bytes);
            if would_be > self.max_bytes {
                // Over budget: admit only the lone oversized job.
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
    /// [`try_reserve`](Self::try_reserve).
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

/// The shared state behind an [`Admission`]. The map is fixed at construction,
/// so it needs no lock: reservations mutate only the scopes' atomics.
#[derive(Debug)]
struct Inner {
    global: Arc<Credits>,
    regions: HashMap<RegionId, Arc<Credits>>,
}

/// The process-wide admission controller. Cheap to [`Clone`] (an [`Arc`]) and
/// shared by every partition worker; all methods take `&self`.
#[derive(Clone, Debug)]
pub struct Admission(Arc<Inner>);

/// Why a scope's ceiling blocked a reservation, qualified by scope. A bounded
/// set suitable as a metric label.
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
/// and ceiling that blocked it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("admission held: {reason}")]
pub struct Held {
    /// The scope and ceiling that blocked the reservation.
    pub reason: HeldReason,
}

/// Why [`Admission::try_admit`] could not return a [`Permit`].
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
/// global scope. Holding a `Permit` is what "the job is outstanding" means;
/// dropping it releases both scopes. Not [`Clone`].
#[derive(Debug)]
pub struct Permit {
    region: Arc<Credits>,
    global: Arc<Credits>,
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
/// `waiting` gauges. An unknown region tracks the global gauge only.
#[derive(Debug)]
pub struct Waiting {
    region: Option<Arc<Credits>>,
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

/// A point-in-time reading of one credit scope, for metrics and logs. `region`
/// is `None` for the global aggregate.
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
    /// Build a controller from `cfg` and the regions it must account for. Only
    /// these regions are admittable; any other id is
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

    /// Try to reserve one job of `bytes` bytes for `region`: the region scope
    /// then the global scope, rolling the region back if the global scope is
    /// full. Returns a [`Permit`] that releases both scopes when dropped, or the
    /// scope and ceiling that blocked it (or [`AdmitError::UnknownRegion`]).
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
            // Global scope full: undo the region reservation.
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
    /// [`Waiting`] guard that clears it on drop. Never affects admission. An
    /// unknown region counts against the global gauge only.
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

    /// A reading of every region scope, in unspecified order.
    pub fn snapshot(&self) -> Vec<RegionUsage> {
        self.0.regions.values().map(|c| c.usage()).collect()
    }

    /// A reading of the process-wide scope (its `region` is `None`).
    pub fn global(&self) -> RegionUsage {
        self.0.global.usage()
    }

    /// Register the admission observable gauges (`jobs_outstanding`,
    /// `admission_waiting`) against `metrics`, read from live [`snapshot`]s. The
    /// returned [`AdmissionGauges`] must be kept alive to keep reporting.
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

    /// Assert a `try_admit` was held for exactly `reason` (`Permit` is not
    /// `PartialEq`, so the `Ok` arm cannot be compared with `assert_eq!`).
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
        let r = region("r");
        let a = controller(
            config(100, 1_000_000, 2, 1_000_000),
            core::slice::from_ref(&r),
        );
        let _p1 = a.try_admit(&r, 1).unwrap();
        let _p2 = a.try_admit(&r, 1).unwrap();
        assert_held(a.try_admit(&r, 1), HeldReason::RegionJobs);
        assert_eq!(a.global().jobs, 2);
    }

    #[test]
    fn region_byte_cap_trips() {
        let r = region("r");
        let a = controller(config(100, 1_000_000, 100, 250), core::slice::from_ref(&r));
        let _p1 = a.try_admit(&r, 200).unwrap();
        assert_held(a.try_admit(&r, 100), HeldReason::RegionBytes);
        assert_eq!(a.snapshot()[0].jobs, 1);
        assert_eq!(a.snapshot()[0].bytes, 200);
    }

    #[test]
    fn global_job_cap_trips_across_two_regions() {
        let a = region("a");
        let b = region("b");
        let adm = controller(config(3, 1_000_000, 10, 1_000_000), &[a.clone(), b.clone()]);
        let _p1 = adm.try_admit(&a, 1).unwrap();
        let _p2 = adm.try_admit(&a, 1).unwrap();
        let _p3 = adm.try_admit(&a, 1).unwrap();
        assert_held(adm.try_admit(&b, 1), HeldReason::GlobalJobs);
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

        let big = a
            .try_admit(&r, 500)
            .expect("lone oversized job is admitted");
        assert_eq!(a.snapshot()[0].bytes, 500);

        assert_held(a.try_admit(&r, 10), HeldReason::RegionBytes);
        assert_held(a.try_admit(&r, 500), HeldReason::RegionBytes);

        drop(big);
        let _small = a.try_admit(&r, 10).expect("fits once the scope is idle");
    }

    #[test]
    fn oversized_job_denied_when_scope_holds_other_work() {
        let r = region("r");
        let a = controller(config(100, 1_000_000, 10, 100), core::slice::from_ref(&r));
        let _small = a.try_admit(&r, 10).unwrap();
        assert_held(a.try_admit(&r, 500), HeldReason::RegionBytes);
    }

    #[test]
    fn oversized_global_job_admitted_when_process_idle() {
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
        const THREADS: usize = 16;
        const ITERS: usize = 2_000;

        let ra = region("ra");
        let rb = region("rb");
        // Tight caps so contention actually reaches them.
        let cfg = config(12, 1_000_000, 8, 1_000_000);
        let a = controller(cfg, &[ra.clone(), rb.clone()]);

        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let a = a.clone();
                let ra = ra.clone();
                let rb = rb.clone();
                scope.spawn(move || {
                    let region = if t % 2 == 0 { &ra } else { &rb };
                    for _ in 0..ITERS {
                        if let Ok(permit) = a.try_admit(region, 1) {
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

        assert_eq!(a.global().jobs, 0);
        assert_eq!(a.global().bytes, 0);
        for u in a.snapshot() {
            assert_eq!(u.jobs, 0, "region {:?} leaked jobs", u.region);
            assert_eq!(u.bytes, 0, "region {:?} leaked bytes", u.region);
        }
    }

    #[test]
    fn concurrent_holds_balance_out() {
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

//! Process-wide byte budget for buffered proxy-metadata reads (#2665).
//!
//! A budgeted read reserves from this budget before the body bytes are
//! allocated. When the length is already known — a cache sidecar `size_bytes`,
//! or an upstream `Content-Length` — the reservation is that length plus a
//! small overhead, not the per-request ceiling. A response with no length
//! reserves the ceiling and returns the unused permits once the body has
//! arrived. Callers that are not inside [`enter`] do not reserve here; the
//! hooks are no-ops so the same body readers stay safe for every other path.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task_local;

/// Default ceiling on the TOTAL bytes the buffered proxy-metadata path may hold
/// resident across ALL in-flight requests (#2665). 1 GiB — eight worst-case
/// 128 MiB buffers, or many more realistically sized ones — chosen so a
/// legitimate concurrent `dnf` refresh does not block while a hostile fan-out
/// cannot drive resident memory unbounded.
///
/// Override with [`PROXY_METADATA_BUDGET_BYTES_ENV`].
pub const DEFAULT_PROXY_METADATA_BUDGET_BYTES: usize = 1024 * 1024 * 1024;

/// Env override for [`DEFAULT_PROXY_METADATA_BUDGET_BYTES`]. A blank,
/// non-numeric, or zero value falls back to the default.
pub const PROXY_METADATA_BUDGET_BYTES_ENV: &str = "AK_PROXY_METADATA_BUDGET_BYTES";

/// Extra permits held above a known body length.
///
/// Covers allocator slack and the short-lived parsed copy a handler keeps
/// beside the raw buffer. It is not a second full copy of a huge document:
/// the reservation is still clamped to the per-request ceiling.
pub const METADATA_RESERVATION_OVERHEAD_BYTES: usize = 1024 * 1024;

/// Permits to take before buffering a body of `body_len` bytes.
///
/// `max` is the per-request ceiling. The result is at least one permit and
/// never more than `max`, so a reservation always fits in a single request's
/// cap and in [`ProxyMetadataBudget::reserve`].
pub fn reservation_for_known_body(body_len: u64, max: usize) -> usize {
    let len = usize::try_from(body_len).unwrap_or(usize::MAX);
    let max = max.max(1);
    len.saturating_add(METADATA_RESERVATION_OVERHEAD_BYTES)
        .clamp(1, max)
}

/// A process-wide byte budget bounding the TOTAL memory the *buffered*
/// proxy-metadata path may hold resident at once, independent of request
/// concurrency (#2665).
pub struct ProxyMetadataBudget {
    sem: Arc<Semaphore>,
    total: usize,
}

impl ProxyMetadataBudget {
    /// Build a budget of `total_bytes`, clamped to `[1, u32::MAX]` (and to
    /// [`Semaphore::MAX_PERMITS`]).
    pub fn new(total_bytes: usize) -> Self {
        let ceiling = (u32::MAX as usize).min(Semaphore::MAX_PERMITS);
        let total = total_bytes.clamp(1, ceiling);
        Self {
            sem: Arc::new(Semaphore::new(total)),
            total,
        }
    }

    /// Total budget in bytes.
    pub fn total_bytes(&self) -> usize {
        self.total
    }

    /// Currently unreserved bytes (observability / test helper).
    pub fn available_bytes(&self) -> usize {
        self.sem.available_permits()
    }

    fn permits_for(&self, bytes: usize) -> u32 {
        // A single request can reserve at most the whole budget, so an oversized
        // request degrades to "hold the whole budget" rather than deadlocking on
        // a permit count the semaphore can never satisfy.
        bytes.clamp(1, self.total) as u32
    }

    /// Reserve `bytes` of the budget, awaiting when it is exhausted. The
    /// returned permit releases the reservation on drop — hold it for as long
    /// as the buffered bytes are resident.
    pub async fn reserve(&self, bytes: usize) -> OwnedSemaphorePermit {
        Arc::clone(&self.sem)
            .acquire_many_owned(self.permits_for(bytes))
            .await
            .expect("proxy metadata budget semaphore is never closed")
    }

    /// Non-blocking reservation: `None` when the budget cannot currently satisfy
    /// `bytes`.
    pub fn try_reserve(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.sem)
            .try_acquire_many_owned(self.permits_for(bytes))
            .ok()
    }
}

/// Process-wide buffered-proxy-metadata byte budget (#2665). Sized once from
/// [`PROXY_METADATA_BUDGET_BYTES_ENV`] (default
/// [`DEFAULT_PROXY_METADATA_BUDGET_BYTES`]); lives for the process lifetime.
pub fn proxy_metadata_budget() -> &'static ProxyMetadataBudget {
    budget_slot().as_ref()
}

fn budget_slot() -> &'static Arc<ProxyMetadataBudget> {
    static BUDGET: OnceLock<Arc<ProxyMetadataBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| {
        let total = std::env::var(PROXY_METADATA_BUDGET_BYTES_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_PROXY_METADATA_BUDGET_BYTES);
        Arc::new(ProxyMetadataBudget::new(total))
    })
}

fn shared_budget() -> Arc<ProxyMetadataBudget> {
    Arc::clone(budget_slot())
}

/// One buffered read's reservation. Held in a task-local for the duration of
/// the fetch so the body reader can reserve before it allocates and the
/// handler can keep the permit until the bytes are dropped.
pub struct MetadataBudgetSession {
    budget: Arc<ProxyMetadataBudget>,
    max: usize,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    /// Permits currently held. Tracked beside the semaphore permit so shrink
    /// and grow do not have to ask the permit for a count.
    held: AtomicUsize,
}

impl MetadataBudgetSession {
    pub fn new(budget: Arc<ProxyMetadataBudget>, max: usize) -> Arc<Self> {
        Arc::new(Self {
            budget,
            max: max.max(1),
            permit: Mutex::new(None),
            held: AtomicUsize::new(0),
        })
    }

    /// Session drawn from the process-wide budget.
    pub fn process(max: usize) -> Arc<Self> {
        Self::new(shared_budget(), max)
    }

    pub fn max(&self) -> usize {
        self.max
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<OwnedSemaphorePermit>> {
        self.permit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drop any permit this session holds before waiting for a new one, so a
    /// request never sits on the semaphore while still occupying it.
    async fn hold(&self, bytes: usize) {
        {
            let mut guard = self.lock();
            *guard = None;
            self.held.store(0, Ordering::Relaxed);
        }
        let permit = self.budget.reserve(bytes).await;
        let acquired = self.budget.permits_for(bytes) as usize;
        let mut guard = self.lock();
        *guard = Some(permit);
        self.held.store(acquired, Ordering::Relaxed);
    }

    fn clear(&self) {
        let mut guard = self.lock();
        *guard = None;
        self.held.store(0, Ordering::Relaxed);
    }

    pub fn take_permit(&self) -> Option<OwnedSemaphorePermit> {
        let permit = self.lock().take();
        if permit.is_some() {
            self.held.store(0, Ordering::Relaxed);
        }
        permit
    }

    /// Shrink to `keep` or try to add the difference. Growing never waits: a
    /// wait while this session already holds permits can deadlock the budget
    /// when every holder needs more than the free remainder.
    fn settle_to(&self, keep: usize) -> bool {
        let keep = keep.clamp(1, self.max);
        let mut guard = self.lock();
        let Some(permit) = guard.as_mut() else {
            return false;
        };
        let held = self.held.load(Ordering::Relaxed);
        if held > keep {
            let extra = permit.split(held - keep);
            drop(extra);
            self.held.store(keep, Ordering::Relaxed);
            return true;
        }
        if held == keep {
            return true;
        }
        let delta = keep - held;
        match self.budget.try_reserve(delta) {
            Some(more) => {
                permit.merge(more);
                self.held.store(keep, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}

task_local! {
    static ACTIVE_SESSION: Arc<MetadataBudgetSession>;
}

fn current() -> Option<Arc<MetadataBudgetSession>> {
    ACTIVE_SESSION.try_with(Arc::clone).ok()
}

/// Run `fut` as a budgeted buffered read. Body readers on this task reserve
/// through [`reserve_known_body`] / [`reserve_upstream_body`].
pub async fn enter<T>(session: Arc<MetadataBudgetSession>, fut: impl Future<Output = T>) -> T {
    ACTIVE_SESSION.scope(session, fut).await
}

pub fn session_active() -> bool {
    current().is_some()
}

/// Reserve `body_len` plus overhead before reading a body whose size is
/// already known. No-op outside [`enter`].
pub async fn reserve_known_body(body_len: u64) {
    let Some(session) = current() else {
        return;
    };
    let bytes = reservation_for_known_body(body_len, session.max);
    session.hold(bytes).await;
}

/// Reserve before an upstream body. A known `Content-Length` reserves that
/// length plus overhead. No length reserves the per-request ceiling; the
/// unused permits are returned by [`settle`]. No-op outside [`enter`].
pub async fn reserve_upstream_body(content_length: Option<u64>) {
    let Some(session) = current() else {
        return;
    };
    let bytes = match content_length {
        Some(len) => reservation_for_known_body(len, session.max),
        None => session.max,
    };
    session.hold(bytes).await;
}

/// `true` when a budgeted read was told the body is larger than its ceiling.
/// Outside a session this is always `false`, so unbudgeted reads keep their
/// existing "read until the cap" behavior.
pub fn upstream_length_exceeds_cap(content_length: Option<u64>) -> bool {
    let Some(session) = current() else {
        return false;
    };
    match content_length {
        Some(len) => usize::try_from(len).unwrap_or(usize::MAX) > session.max,
        None => false,
    }
}

/// Byte cap for the upstream read loop while a session is active.
///
/// Equals the reservation just taken, so the loop cannot buffer more than the
/// permits this request holds. `None` outside a session (the caller uses its
/// own ceiling).
pub fn active_read_cap() -> Option<usize> {
    let session = current()?;
    let held = session.held.load(Ordering::Relaxed);
    if held == 0 {
        None
    } else {
        Some(held.min(session.max))
    }
}

/// After the body is in hand, keep permits for `actual_len` plus overhead and
/// return the rest. `true` when the reservation covers the body (or when no
/// session is active). `false` when growing would have to wait; the caller
/// must drop the body.
pub fn settle(actual_len: usize) -> bool {
    let Some(session) = current() else {
        return true;
    };
    let keep = reservation_for_known_body(actual_len as u64, session.max);
    session.settle_to(keep)
}

/// Return this request's permits. A miss calls this before any later read so
/// the next reservation does not stack on top of the one it just abandoned.
pub fn release() {
    if let Some(session) = current() {
        session.clear();
    }
}

#[cfg(ak_test_shard = "services-1")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_body_reserves_length_plus_overhead_not_the_ceiling() {
        let ceiling = 128 * 1024 * 1024;
        let reserved = reservation_for_known_body(200_000, ceiling);
        assert_eq!(reserved, 200_000 + METADATA_RESERVATION_OVERHEAD_BYTES);
        assert!(reserved < ceiling);
    }

    #[test]
    fn known_body_reservation_never_exceeds_the_request_ceiling() {
        assert_eq!(reservation_for_known_body(50_000_000, 1024), 1024);
    }

    #[tokio::test]
    async fn cache_sized_reservation_is_taken_before_the_body_read() {
        let total = 8 * 1024 * 1024;
        let budget = Arc::new(ProxyMetadataBudget::new(total));
        let session = MetadataBudgetSession::new(budget.clone(), 128 * 1024 * 1024);
        let reserved_before_read = enter(session, async {
            reserve_known_body(200_000).await;
            let reserved = total - budget.available_bytes();
            // The body read would happen here. The permits are already held.
            assert!(settle(200_000));
            reserved
        })
        .await;
        assert_eq!(
            reserved_before_read,
            200_000 + METADATA_RESERVATION_OVERHEAD_BYTES
        );
    }

    #[tokio::test]
    async fn content_length_reserves_before_the_body_and_unknown_length_shrinks_after() {
        let total = 4 * 1024 * 1024;
        let max = 1024 * 1024;
        let budget = Arc::new(ProxyMetadataBudget::new(total));
        let session = MetadataBudgetSession::new(budget.clone(), max);

        enter(Arc::clone(&session), async {
            reserve_upstream_body(Some(4096)).await;
            assert_eq!(
                total - budget.available_bytes(),
                reservation_for_known_body(4096, max)
            );
            assert_eq!(
                active_read_cap(),
                Some(reservation_for_known_body(4096, max))
            );
            release();

            reserve_upstream_body(None).await;
            assert_eq!(total - budget.available_bytes(), max);
            assert!(settle(100));
            assert_eq!(
                total - budget.available_bytes(),
                reservation_for_known_body(100, max)
            );
        })
        .await;
    }

    #[tokio::test]
    async fn advertised_length_over_the_ceiling_is_refused_before_a_reservation() {
        let budget = Arc::new(ProxyMetadataBudget::new(1024 * 1024));
        let session = MetadataBudgetSession::new(budget.clone(), 64 * 1024);
        enter(session, async {
            assert!(upstream_length_exceeds_cap(Some(64 * 1024 + 1)));
            assert!(!upstream_length_exceeds_cap(Some(64 * 1024)));
            assert!(!upstream_length_exceeds_cap(None));
            assert_eq!(budget.available_bytes(), budget.total_bytes());
        })
        .await;
        assert!(!upstream_length_exceeds_cap(Some(u64::MAX)));
    }

    #[tokio::test]
    async fn growing_a_reservation_does_not_wait_when_the_budget_is_full() {
        let overhead = METADATA_RESERVATION_OVERHEAD_BYTES;
        let total = 100 + overhead;
        let budget = Arc::new(ProxyMetadataBudget::new(total));
        let session = MetadataBudgetSession::new(budget.clone(), total * 4);
        let fitted = enter(session, async {
            reserve_known_body(100).await;
            // The body is larger than the reservation and nothing else is free.
            // Waiting here would deadlock; the read must fail instead.
            let ok = settle(100 + overhead + 1);
            let still_held = budget.available_bytes() == 0;
            ok && still_held
        })
        .await;
        assert!(!fitted);
        assert_eq!(budget.available_bytes(), total);
    }

    #[test]
    fn a_one_gib_budget_admits_more_than_eight_realistically_sized_packuments() {
        let budget = ProxyMetadataBudget::new(DEFAULT_PROXY_METADATA_BUDGET_BYTES);
        let packument = reservation_for_known_body(200_000, 128 * 1024 * 1024);
        let mut held = Vec::new();
        for _ in 0..64 {
            held.push(
                budget
                    .try_reserve(packument)
                    .expect("a small packument must not consume a 128 MiB slot"),
            );
        }
        assert!(held.len() > 8);
        assert!(
            budget.try_reserve(128 * 1024 * 1024).is_some(),
            "64 small reservations must leave a worst-case slot free in 1 GiB"
        );
    }
}

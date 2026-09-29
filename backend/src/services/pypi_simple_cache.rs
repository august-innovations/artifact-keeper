//! In-process cache of a finished PyPI simple-project page.
//!
//! `GET /pypi/<repo>/simple/<project>/` rebuilds the menu a client reads
//! before it downloads a file: URL rewrite, the age gate, and the virtual
//! merge. That rebuild queries Aurora even when the S3 proxy cache already
//! holds the raw upstream index. This cache stores the finished body so a
//! resolve storm reuses it.
//!
//! One process, one cache. A publish or a version crossing the age gate can
//! be stale until [`PYPI_SIMPLE_CACHE_TTL_DEFAULT_SECS`] expires. Stored
//! bodies are capped at [`PYPI_SIMPLE_CACHE_MAX_BYTES`]. File bytes,
//! `.whl.metadata`, the `/simple/` root index, and error responses are not
//! stored here.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::watch;
use uuid::Uuid;

/// `PYPI_SIMPLE_CACHE_TTL_SECS` when the variable is unset. Five minutes.
pub const PYPI_SIMPLE_CACHE_TTL_DEFAULT_SECS: u64 = 300;

/// `PYPI_SIMPLE_CACHE_MAX_BYTES` when the variable is unset. 64 MiB of stored
/// page bodies per process, weighed by body length the same way the resolver
/// index memo is weighed: a popular project's menu is megabytes, so an
/// entry-count bound would not bound memory.
pub const PYPI_SIMPLE_CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Which simple-project representation the caller asked for.
///
/// HTML and JSON are separate entries. The key is the request, not the
/// upstream's content type: a JSON request whose upstream answered HTML is
/// still stored under [`SimpleRepresentation::Json`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SimpleRepresentation {
    Html,
    Json,
}

/// Whether this request may use the cache.
///
/// `Disabled` is `PYPI_SIMPLE_CACHE_TTL_SECS=0` and wins over [`Self::Bypass`]:
/// the cache is off for the whole process. `Bypass` is any repository whose
/// format is not PyPI (poetry, jupyter, conda) on this route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Disabled,
    Bypass,
    Enabled,
}

/// Map the operator TTL and the repository format onto [`CacheMode`].
pub fn simple_cache_decision(ttl_secs: u64, format: &str) -> CacheMode {
    if ttl_secs == 0 {
        CacheMode::Disabled
    } else if format.eq_ignore_ascii_case("pypi") {
        CacheMode::Enabled
    } else {
        CacheMode::Bypass
    }
}

/// Parse `PYPI_SIMPLE_CACHE_TTL_SECS`.
///
/// Unset means [`PYPI_SIMPLE_CACHE_TTL_DEFAULT_SECS`]. `0` is valid and
/// disables the cache. An empty, negative, or non-integer value is a startup
/// error: a typo must not silently fall back to the default or to "off".
pub fn parse_pypi_simple_cache_ttl(raw: Option<&str>) -> Result<u64, String> {
    let Some(raw) = raw else {
        return Ok(PYPI_SIMPLE_CACHE_TTL_DEFAULT_SECS);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(format!(
            "PYPI_SIMPLE_CACHE_TTL_SECS is set but empty; omit it to use {PYPI_SIMPLE_CACHE_TTL_DEFAULT_SECS} seconds, or set 0 to disable the cache"
        ));
    }
    if raw.starts_with('+') || raw.starts_with('-') {
        return Err(format!(
            "PYPI_SIMPLE_CACHE_TTL_SECS must be a non-negative integer (0 disables the cache), got {raw:?}"
        ));
    }
    raw.parse::<u64>().map_err(|_| {
        format!(
            "PYPI_SIMPLE_CACHE_TTL_SECS must be a non-negative integer (0 disables the cache), got {raw:?}"
        )
    })
}

/// Parse `PYPI_SIMPLE_CACHE_MAX_BYTES`.
///
/// Unset means [`PYPI_SIMPLE_CACHE_MAX_BYTES`] (64 MiB). `0` stores nothing:
/// every non-empty page is larger than the cap and is served without being
/// kept. An empty, negative, or non-integer value refuses to start.
pub fn parse_pypi_simple_cache_max_bytes(raw: Option<&str>) -> Result<u64, String> {
    let Some(raw) = raw else {
        return Ok(PYPI_SIMPLE_CACHE_MAX_BYTES);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(
            "PYPI_SIMPLE_CACHE_MAX_BYTES is set but empty; omit it to use 64 MiB, or set 0 to store nothing"
                .to_string(),
        );
    }
    if raw.starts_with('+') || raw.starts_with('-') {
        return Err(format!(
            "PYPI_SIMPLE_CACHE_MAX_BYTES must be a non-negative integer (bytes of stored page bodies), got {raw:?}"
        ));
    }
    raw.parse::<u64>().map_err(|_| {
        format!(
            "PYPI_SIMPLE_CACHE_MAX_BYTES must be a non-negative integer (bytes of stored page bodies), got {raw:?}"
        )
    })
}

/// Cache key: repository, PEP 503 project name, representation, and the set
/// of virtual members this caller may read.
///
/// Hosted and remote repositories use an empty member set — one fixed scope.
/// A virtual repository uses the authorized member ids. Order does not matter;
/// [`PypiSimpleCacheKey::new`] sorts them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PypiSimpleCacheKey {
    repository_id: Uuid,
    project: String,
    representation: SimpleRepresentation,
    caller_scope: Vec<Uuid>,
}

impl PypiSimpleCacheKey {
    pub fn new(
        repository_id: Uuid,
        project: impl Into<String>,
        representation: SimpleRepresentation,
        mut caller_scope: Vec<Uuid>,
    ) -> Self {
        caller_scope.sort();
        caller_scope.dedup();
        Self {
            repository_id,
            project: project.into(),
            representation,
            caller_scope,
        }
    }
}

/// A finished simple-project page, after rewrite, the age gate, and the merge.
#[derive(Debug, Clone)]
pub struct CachedSimplePage {
    pub body: Bytes,
    pub content_type: String,
}

/// What a lookup did, as the `result` label on the cache counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupKind {
    Hit,
    Miss,
    Disabled,
    Bypass,
}

impl LookupKind {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Disabled => "disabled",
            Self::Bypass => "bypass",
        }
    }
}

/// A build that failed. Not stored. Waiters of the same in-flight build share
/// this value; the next request builds again.
///
/// The concrete error stays with the caller. The cache only needs to move it
/// to other tasks that joined the same key.
pub struct BuildFailure(Arc<dyn Any + Send + Sync>);

impl Clone for BuildFailure {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl BuildFailure {
    fn from_value<E: Send + Sync + 'static>(err: E) -> Self {
        Self(Arc::new(err))
    }

    /// Clone the error the build returned.
    ///
    /// Panics if `E` is not the type that was stored. One cache serves one
    /// build error type; a mismatch is a programming bug.
    pub fn downcast<E: Clone + Send + Sync + 'static>(&self) -> E {
        self.0
            .downcast_ref::<E>()
            .unwrap_or_else(|| {
                panic!(
                    "pypi simple cache build failure was not {}",
                    std::any::type_name::<E>()
                )
            })
            .clone()
    }
}

/// The page, or the build's error.
pub enum LookupOutcome<E> {
    Ready(Arc<CachedSimplePage>),
    Failed(E),
}

pub struct Lookup<E> {
    pub kind: LookupKind,
    pub outcome: LookupOutcome<E>,
}

type FlightResult = Result<Arc<CachedSimplePage>, BuildFailure>;

struct Entry {
    page: Arc<CachedSimplePage>,
    expires_at: Instant,
    seq: u64,
    weight: u64,
}

struct Inner {
    entries: HashMap<PypiSimpleCacheKey, Entry>,
    /// Insertion order. The smallest sequence is the oldest.
    by_age: BTreeMap<u64, PypiSimpleCacheKey>,
    next_seq: u64,
    total_bytes: u64,
    inflight: HashMap<PypiSimpleCacheKey, watch::Receiver<Option<FlightResult>>>,
}

/// In-process simple-project cache.
///
/// One in-flight build per key; waiters share that result. Expired entries
/// are dropped before anything else, then the oldest insertion, until the
/// stored bodies fit in the configured byte cap
/// ([`PYPI_SIMPLE_CACHE_MAX_BYTES`] unless `PYPI_SIMPLE_CACHE_MAX_BYTES` is set).
pub struct PypiSimpleCache {
    ttl: Duration,
    max_bytes: u64,
    now: Arc<dyn Fn() -> Instant + Send + Sync>,
    inner: Mutex<Inner>,
    /// Waiters that joined an in-flight build. The single-flight test waits
    /// on this so it can release the leader only after the waiter is attached.
    joined_waiters: AtomicUsize,
}

impl PypiSimpleCache {
    pub fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, PYPI_SIMPLE_CACHE_MAX_BYTES)
    }

    /// `max_bytes` is the cap on stored page bodies. A page larger than the
    /// cap is returned to the caller and not inserted.
    pub fn with_capacity(ttl: Duration, max_bytes: u64) -> Self {
        Self::with_clock(ttl, max_bytes, Arc::new(Instant::now))
    }

    fn with_clock(
        ttl: Duration,
        max_bytes: u64,
        now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self {
            ttl,
            max_bytes,
            now,
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                by_age: BTreeMap::new(),
                next_seq: 0,
                total_bytes: 0,
                inflight: HashMap::new(),
            }),
            joined_waiters: AtomicUsize::new(0),
        }
    }

    /// Look up `key`, or build it.
    ///
    /// [`CacheMode::Disabled`] and [`CacheMode::Bypass`] call `build` every
    /// time and store nothing. [`CacheMode::Enabled`] returns a stored page
    /// without calling `build`, and single-flights a miss. A failed build is
    /// not stored.
    pub async fn lookup<E, F, Fut>(
        &self,
        mode: CacheMode,
        key: PypiSimpleCacheKey,
        build: F,
    ) -> Lookup<E>
    where
        E: Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<CachedSimplePage, E>> + Send,
    {
        match mode {
            CacheMode::Disabled => self.run_uncached(LookupKind::Disabled, build).await,
            CacheMode::Bypass => self.run_uncached(LookupKind::Bypass, build).await,
            CacheMode::Enabled => self.lookup_enabled(key, build).await,
        }
    }

    async fn run_uncached<E, F, Fut>(&self, kind: LookupKind, build: F) -> Lookup<E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<CachedSimplePage, E>> + Send,
    {
        match build().await {
            Ok(page) => Lookup {
                kind,
                outcome: LookupOutcome::Ready(Arc::new(page)),
            },
            Err(err) => Lookup {
                kind,
                outcome: LookupOutcome::Failed(err),
            },
        }
    }

    async fn lookup_enabled<E, F, Fut>(&self, key: PypiSimpleCacheKey, build: F) -> Lookup<E>
    where
        E: Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<CachedSimplePage, E>> + Send,
    {
        let mut build = Some(build);
        loop {
            enum Role {
                Hit(Arc<CachedSimplePage>),
                Wait(watch::Receiver<Option<FlightResult>>),
                Lead(watch::Sender<Option<FlightResult>>),
            }

            let now = (self.now)();
            let role = {
                let mut guard = self.lock();
                let waiting = guard.inflight.get(&key).cloned();
                if let Some(page) = fresh_page(&mut guard, &key, now) {
                    Role::Hit(page)
                } else if let Some(rx) = waiting {
                    drop(guard);
                    self.joined_waiters.fetch_add(1, Ordering::SeqCst);
                    Role::Wait(rx)
                } else {
                    let (tx, rx) = watch::channel(None);
                    guard.inflight.insert(key.clone(), rx);
                    Role::Lead(tx)
                }
            };

            match role {
                Role::Hit(page) => {
                    return Lookup {
                        kind: LookupKind::Hit,
                        outcome: LookupOutcome::Ready(page),
                    };
                }
                Role::Wait(rx) => match wait_for_flight(rx).await {
                    Some(Ok(page)) => {
                        return Lookup {
                            kind: LookupKind::Miss,
                            outcome: LookupOutcome::Ready(page),
                        };
                    }
                    Some(Err(failure)) => {
                        return Lookup {
                            kind: LookupKind::Miss,
                            outcome: LookupOutcome::Failed(failure.downcast::<E>()),
                        };
                    }
                    // The leader disappeared before publishing. Try again.
                    // This task has not built yet, so it may become the leader.
                    None => continue,
                },
                Role::Lead(tx) => {
                    let _clear = InflightGuard {
                        cache: self,
                        key: key.clone(),
                    };
                    let built = build.take().expect("leader builds once")().await;
                    let shared = match built {
                        Ok(page) => {
                            let page = Arc::new(page);
                            self.store(key.clone(), Arc::clone(&page));
                            Ok(page)
                        }
                        Err(err) => Err(BuildFailure::from_value(err)),
                    };
                    let _ = tx.send(Some(shared.clone()));
                    return match shared {
                        Ok(page) => Lookup {
                            kind: LookupKind::Miss,
                            outcome: LookupOutcome::Ready(page),
                        },
                        Err(failure) => Lookup {
                            kind: LookupKind::Miss,
                            outcome: LookupOutcome::Failed(failure.downcast::<E>()),
                        },
                    };
                }
            }
        }
    }

    fn store(&self, key: PypiSimpleCacheKey, page: Arc<CachedSimplePage>) {
        let ttl = self.ttl;
        let max_bytes = self.max_bytes;
        let now = (self.now)();
        let mut guard = self.lock();
        sweep_expired(&mut guard, now);
        let weight = page.body.len() as u64;
        if weight > max_bytes {
            tracing::debug!(
                bytes = weight,
                cap = max_bytes,
                "pypi simple cache skipped a page larger than the memory cap"
            );
            return;
        }
        // A refresh of a key that is still present replaces it in place so
        // the byte total does not count the body twice.
        remove_entry(&mut guard, &key);
        while guard.total_bytes + weight > max_bytes {
            if !evict_oldest(&mut guard) {
                return;
            }
        }
        let seq = guard.next_seq;
        guard.next_seq = guard.next_seq.saturating_add(1);
        guard.total_bytes += weight;
        guard.by_age.insert(seq, key.clone());
        guard.entries.insert(
            key,
            Entry {
                page,
                expires_at: now + ttl,
                seq,
                weight,
            },
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    fn stored_body_bytes(&self) -> u64 {
        self.lock().total_bytes
    }
}

/// Removes the in-flight slot if the leader unwinds before publishing.
struct InflightGuard<'a> {
    cache: &'a PypiSimpleCache,
    key: PypiSimpleCacheKey,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.cache.lock().inflight.remove(&self.key);
    }
}

async fn wait_for_flight(mut rx: watch::Receiver<Option<FlightResult>>) -> Option<FlightResult> {
    loop {
        if let Some(result) = rx.borrow().clone() {
            return Some(result);
        }
        if rx.changed().await.is_err() {
            return None;
        }
    }
}

fn fresh_page(
    guard: &mut Inner,
    key: &PypiSimpleCacheKey,
    now: Instant,
) -> Option<Arc<CachedSimplePage>> {
    let expired = guard
        .entries
        .get(key)
        .is_some_and(|entry| entry.expires_at <= now);
    if expired {
        remove_entry(guard, key);
        None
    } else {
        guard.entries.get(key).map(|entry| Arc::clone(&entry.page))
    }
}

fn sweep_expired(guard: &mut Inner, now: Instant) {
    let expired: Vec<PypiSimpleCacheKey> = guard
        .entries
        .iter()
        .filter(|(_, entry)| entry.expires_at <= now)
        .map(|(key, _)| key.clone())
        .collect();
    for key in expired {
        remove_entry(guard, &key);
    }
}

fn evict_oldest(guard: &mut Inner) -> bool {
    let Some(seq) = guard.by_age.keys().next().copied() else {
        return false;
    };
    let Some(key) = guard.by_age.get(&seq).cloned() else {
        return false;
    };
    remove_entry(guard, &key);
    true
}

fn remove_entry(guard: &mut Inner, key: &PypiSimpleCacheKey) {
    let Some(entry) = guard.entries.remove(key) else {
        return;
    };
    guard.by_age.remove(&entry.seq);
    guard.total_bytes = guard.total_bytes.saturating_sub(entry.weight);
}

#[cfg(ak_test_shard = "services-1")]
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn page(body: &'static [u8]) -> CachedSimplePage {
        CachedSimplePage {
            body: Bytes::from_static(body),
            content_type: "text/html; charset=utf-8".to_string(),
        }
    }

    fn key(
        repo: u128,
        project: &str,
        representation: SimpleRepresentation,
        scope: &[u128],
    ) -> PypiSimpleCacheKey {
        PypiSimpleCacheKey::new(
            Uuid::from_u128(repo),
            project,
            representation,
            scope.iter().copied().map(Uuid::from_u128).collect(),
        )
    }

    struct ManualClock {
        base: Instant,
        offset_ms: Arc<AtomicUsize>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset_ms: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn advance(&self, by: Duration) {
            self.offset_ms
                .fetch_add(by.as_millis() as usize, Ordering::Relaxed);
        }

        fn handle(&self) -> Arc<dyn Fn() -> Instant + Send + Sync> {
            let base = self.base;
            let offset_ms = Arc::clone(&self.offset_ms);
            Arc::new(move || base + Duration::from_millis(offset_ms.load(Ordering::Relaxed) as u64))
        }
    }

    fn cache_with(ttl: Duration, max_bytes: u64, clock: &ManualClock) -> PypiSimpleCache {
        PypiSimpleCache::with_clock(ttl, max_bytes, clock.handle())
    }

    /// The handler passes the artifacts query as this build. A hit must not run it.
    #[tokio::test]
    async fn second_lookup_for_the_same_key_does_not_rebuild() {
        let cache = PypiSimpleCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        let k = key(1, "requests", SimpleRepresentation::Html, &[]);

        for expected in [LookupKind::Miss, LookupKind::Hit] {
            let calls = Arc::clone(&calls);
            let lookup = cache
                .lookup(CacheMode::Enabled, k.clone(), || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(page(b"<html>requests</html>"))
                    }
                })
                .await;
            assert_eq!(lookup.kind, expected);
            match lookup.outcome {
                LookupOutcome::Ready(page) => assert_eq!(&page.body[..], b"<html>requests</html>"),
                LookupOutcome::Failed(_) => panic!("build failed"),
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn html_and_json_are_different_entries() {
        let cache = PypiSimpleCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        for representation in [SimpleRepresentation::Html, SimpleRepresentation::Json] {
            let calls = Arc::clone(&calls);
            let lookup = cache
                .lookup(
                    CacheMode::Enabled,
                    key(1, "requests", representation, &[]),
                    || {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, String>(page(b"body"))
                        }
                    },
                )
                .await;
            assert_eq!(lookup.kind, LookupKind::Miss);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let calls_before = calls.load(Ordering::SeqCst);
        let again = cache
            .lookup(
                CacheMode::Enabled,
                key(1, "requests", SimpleRepresentation::Html, &[]),
                || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(page(b"body"))
                    }
                },
            )
            .await;
        assert_eq!(again.kind, LookupKind::Hit);
        assert_eq!(calls.load(Ordering::SeqCst), calls_before);
    }

    #[tokio::test]
    async fn distinct_caller_scopes_do_not_share_an_entry() {
        let cache = PypiSimpleCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        for scope in [&[1u128][..], &[1, 2][..]] {
            let calls = Arc::clone(&calls);
            let lookup = cache
                .lookup(
                    CacheMode::Enabled,
                    key(7, "private-pkg", SimpleRepresentation::Html, scope),
                    || {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, String>(page(b"scoped"))
                        }
                    },
                )
                .await;
            assert_eq!(lookup.kind, LookupKind::Miss);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_same_member_set_in_either_order_shares_an_entry() {
        let cache = PypiSimpleCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        for scope in [&[2u128, 1][..], &[1, 2][..]] {
            let calls = Arc::clone(&calls);
            let _ = cache
                .lookup(
                    CacheMode::Enabled,
                    key(7, "private-pkg", SimpleRepresentation::Json, scope),
                    || {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, String>(page(b"same-set"))
                        }
                    },
                )
                .await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_repositories_do_not_share_an_entry() {
        let cache = PypiSimpleCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        for repo in [1u128, 2] {
            let calls = Arc::clone(&calls);
            let lookup = cache
                .lookup(
                    CacheMode::Enabled,
                    key(repo, "requests", SimpleRepresentation::Html, &[]),
                    || {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, String>(page(b"repo"))
                        }
                    },
                )
                .await;
            assert_eq!(lookup.kind, LookupKind::Miss);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn disabled_mode_always_rebuilds() {
        let cache = PypiSimpleCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        let k = key(1, "requests", SimpleRepresentation::Html, &[]);
        for _ in 0..2 {
            let calls = Arc::clone(&calls);
            let lookup = cache
                .lookup(CacheMode::Disabled, k.clone(), || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(page(b"uncached"))
                    }
                })
                .await;
            assert_eq!(lookup.kind, LookupKind::Disabled);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.stored_body_bytes(), 0);
    }

    #[tokio::test]
    async fn bypass_mode_always_rebuilds() {
        let cache = PypiSimpleCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        let k = key(1, "requests", SimpleRepresentation::Html, &[]);
        for _ in 0..2 {
            let calls = Arc::clone(&calls);
            let lookup = cache
                .lookup(CacheMode::Bypass, k.clone(), || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(page(b"poetry"))
                    }
                })
                .await;
            assert_eq!(lookup.kind, LookupKind::Bypass);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_build_is_not_stored() {
        let cache = PypiSimpleCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        let k = key(1, "missing", SimpleRepresentation::Html, &[]);
        for _ in 0..2 {
            let calls = Arc::clone(&calls);
            let lookup = cache
                .lookup(CacheMode::Enabled, k.clone(), || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err::<CachedSimplePage, String>("package not found".to_string())
                    }
                })
                .await;
            assert_eq!(lookup.kind, LookupKind::Miss);
            match lookup.outcome {
                LookupOutcome::Failed(err) => assert_eq!(err, "package not found"),
                LookupOutcome::Ready(_) => panic!("a failed build was stored"),
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.stored_body_bytes(), 0);
    }

    #[tokio::test]
    async fn concurrent_lookups_for_one_key_build_once() {
        let cache = Arc::new(PypiSimpleCache::new(Duration::from_secs(60)));
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let k = key(1, "urllib3", SimpleRepresentation::Json, &[]);

        let leader = {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            let k = k.clone();
            tokio::spawn(async move {
                cache
                    .lookup(CacheMode::Enabled, k, || {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            let _ = started_tx.send(());
                            // A oneshot keeps the release even if it arrives
                            // before this await, so the wakeup cannot be lost.
                            let _ = release_rx.await;
                            Ok::<_, String>(page(b"shared"))
                        }
                    })
                    .await
            })
        };

        started_rx.await.unwrap();
        let waiter = {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                cache
                    .lookup(CacheMode::Enabled, k, || {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, String>(page(b"should-not-run"))
                        }
                    })
                    .await
            })
        };
        while cache.joined_waiters.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        release_tx.send(()).unwrap();

        let leader = leader.await.unwrap();
        let waiter = waiter.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(leader.kind, LookupKind::Miss);
        assert_eq!(waiter.kind, LookupKind::Miss);
        match waiter.outcome {
            LookupOutcome::Ready(page) => assert_eq!(&page.body[..], b"shared"),
            LookupOutcome::Failed(_) => panic!("waiter did not share the leader's page"),
        }
    }

    #[tokio::test]
    async fn an_expired_entry_is_rebuilt() {
        let clock = ManualClock::new();
        let cache = cache_with(Duration::from_secs(10), 1024, &clock);
        let k = key(1, "certifi", SimpleRepresentation::Html, &[]);
        let calls = Arc::new(AtomicUsize::new(0));

        let calls_hit = Arc::clone(&calls);
        let first = cache
            .lookup(CacheMode::Enabled, k.clone(), || {
                let calls_hit = Arc::clone(&calls_hit);
                async move {
                    calls_hit.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, String>(page(b"v1"))
                }
            })
            .await;
        assert_eq!(first.kind, LookupKind::Miss);

        clock.advance(Duration::from_secs(10));
        let calls_hit = Arc::clone(&calls);
        let second = cache
            .lookup(CacheMode::Enabled, k, || {
                let calls_hit = Arc::clone(&calls_hit);
                async move {
                    calls_hit.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, String>(page(b"v2"))
                }
            })
            .await;
        assert_eq!(second.kind, LookupKind::Miss);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    async fn insert_body(cache: &PypiSimpleCache, name: &str, byte: u8, len: usize) {
        let body = Bytes::from(vec![byte; len]);
        let k = key(1, name, SimpleRepresentation::Html, &[]);
        let _ = cache
            .lookup(CacheMode::Enabled, k, || async move {
                Ok::<_, String>(CachedSimplePage {
                    body,
                    content_type: "text/html".to_string(),
                })
            })
            .await;
    }

    #[tokio::test]
    async fn expired_entries_are_dropped_before_fresh_ones() {
        let clock = ManualClock::new();
        let cache = cache_with(Duration::from_secs(100), 200, &clock);

        insert_body(&cache, "old-a", b'a', 50).await;
        insert_body(&cache, "old-b", b'b', 50).await;
        clock.advance(Duration::from_secs(50));
        insert_body(&cache, "fresh", b'c', 50).await;
        assert_eq!(cache.stored_body_bytes(), 150);

        clock.advance(Duration::from_secs(70));
        // old-a and old-b have expired. fresh has not. The new page fits
        // beside fresh only because the expired bodies are dropped first.
        insert_body(&cache, "new", b'd', 50).await;
        assert_eq!(cache.stored_body_bytes(), 100);

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_hit = Arc::clone(&calls);
        let old = cache
            .lookup(
                CacheMode::Enabled,
                key(1, "old-a", SimpleRepresentation::Html, &[]),
                || {
                    let calls_hit = Arc::clone(&calls_hit);
                    async move {
                        calls_hit.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(page(b"rebuilt"))
                    }
                },
            )
            .await;
        assert_eq!(old.kind, LookupKind::Miss);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_oldest_entry_is_evicted_when_bodies_exceed_the_cap() {
        let clock = ManualClock::new();
        let cache = cache_with(Duration::from_secs(60), 100, &clock);
        let calls = Arc::new(AtomicUsize::new(0));
        for (name, byte) in [("oldest", b'a'), ("middle", b'b'), ("newest", b'c')] {
            let calls = Arc::clone(&calls);
            let body = Bytes::from(vec![byte; 40]);
            let _ = cache
                .lookup(
                    CacheMode::Enabled,
                    key(1, name, SimpleRepresentation::Html, &[]),
                    || {
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, String>(CachedSimplePage {
                                body,
                                content_type: "text/html".to_string(),
                            })
                        }
                    },
                )
                .await;
        }
        // 40 + 40 + 40 does not fit in 100. The oldest body is the one dropped.
        assert!(cache.stored_body_bytes() <= 100);

        let calls_before = calls.load(Ordering::SeqCst);
        let oldest = cache
            .lookup(
                CacheMode::Enabled,
                key(1, "oldest", SimpleRepresentation::Html, &[]),
                || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(page(b"rebuilt-oldest"))
                    }
                },
            )
            .await;
        assert_eq!(oldest.kind, LookupKind::Miss);
        assert_eq!(calls.load(Ordering::SeqCst), calls_before + 1);

        let middle = cache
            .lookup(
                CacheMode::Enabled,
                key(1, "middle", SimpleRepresentation::Html, &[]),
                || async { Ok::<_, String>(page(b"should-hit")) },
            )
            .await;
        assert_eq!(middle.kind, LookupKind::Hit);
    }

    #[tokio::test]
    async fn a_page_larger_than_the_cap_is_not_stored() {
        let clock = ManualClock::new();
        let cache = cache_with(Duration::from_secs(60), 8, &clock);
        let lookup = cache
            .lookup(
                CacheMode::Enabled,
                key(1, "huge", SimpleRepresentation::Html, &[]),
                || async { Ok::<_, String>(page(b"0123456789")) },
            )
            .await;
        assert_eq!(lookup.kind, LookupKind::Miss);
        assert_eq!(cache.stored_body_bytes(), 0);

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_hit = Arc::clone(&calls);
        let again = cache
            .lookup(
                CacheMode::Enabled,
                key(1, "huge", SimpleRepresentation::Html, &[]),
                || {
                    let calls_hit = Arc::clone(&calls_hit);
                    async move {
                        calls_hit.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, String>(page(b"0123456789"))
                    }
                },
            )
            .await;
        assert_eq!(again.kind, LookupKind::Miss);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ttl_zero_is_disabled_even_for_a_pypi_repository() {
        assert_eq!(simple_cache_decision(0, "pypi"), CacheMode::Disabled);
        assert_eq!(simple_cache_decision(0, "poetry"), CacheMode::Disabled);
    }

    #[test]
    fn a_non_pypi_format_bypasses_an_enabled_cache() {
        assert_eq!(simple_cache_decision(60, "pypi"), CacheMode::Enabled);
        assert_eq!(simple_cache_decision(60, "PyPI"), CacheMode::Enabled);
        assert_eq!(simple_cache_decision(60, "poetry"), CacheMode::Bypass);
        assert_eq!(simple_cache_decision(60, "conda"), CacheMode::Bypass);
        assert_eq!(simple_cache_decision(60, "jupyter"), CacheMode::Bypass);
    }

    #[test]
    fn parse_ttl_defaults_when_unset_and_rejects_garbage() {
        assert_eq!(parse_pypi_simple_cache_ttl(None).unwrap(), 300);
        assert_eq!(parse_pypi_simple_cache_ttl(Some("60")).unwrap(), 60);
        assert_eq!(parse_pypi_simple_cache_ttl(Some("300")).unwrap(), 300);
        assert_eq!(parse_pypi_simple_cache_ttl(Some(" 0 ")).unwrap(), 0);
        let empty = parse_pypi_simple_cache_ttl(Some("")).unwrap_err();
        assert!(
            empty.contains(&PYPI_SIMPLE_CACHE_TTL_DEFAULT_SECS.to_string()),
            "{empty}"
        );
        assert!(parse_pypi_simple_cache_ttl(Some("-1")).is_err());
        assert!(parse_pypi_simple_cache_ttl(Some("+60")).is_err());
        assert!(parse_pypi_simple_cache_ttl(Some("60s")).is_err());
        assert!(parse_pypi_simple_cache_ttl(Some("1.5")).is_err());
    }

    #[test]
    fn parse_max_bytes_defaults_to_64_mib_and_rejects_garbage() {
        assert_eq!(
            parse_pypi_simple_cache_max_bytes(None).unwrap(),
            64 * 1024 * 1024
        );
        assert_eq!(
            parse_pypi_simple_cache_max_bytes(Some("1024")).unwrap(),
            1024
        );
        assert_eq!(parse_pypi_simple_cache_max_bytes(Some(" 0 ")).unwrap(), 0);
        assert!(parse_pypi_simple_cache_max_bytes(Some("")).is_err());
        assert!(parse_pypi_simple_cache_max_bytes(Some("-1")).is_err());
        assert!(parse_pypi_simple_cache_max_bytes(Some("64MiB")).is_err());
    }
}

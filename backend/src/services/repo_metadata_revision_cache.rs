//! Format-agnostic, revision-validated cache for rendered repository
//! metadata (#4174).
//!
//! Extracted from the RPM repodata cache (#2521, PF-004), which is its first
//! consumer; conda `repodata.json` is the second (#4172). A format plugs in
//! three things:
//!
//! * a **fingerprint** of the repository state a render is built from,
//!   computed per request by one cheap aggregate query (RPM: `COUNT(*)` and
//!   `MAX(updated_at)` over the live `.rpm` rows). Any [`Fingerprint`] that is
//!   `Eq` works; [`Fingerprint::partition`] names the caller-visibility class
//!   a render describes (RPM: the authorized member-id set of a virtual,
//!   #4346), so callers that are owed different documents never share an
//!   entry;
//! * a **render** closure that builds the complete document set for one
//!   state ([`CachedRender`] reports its byte weight and whether it may be
//!   kept);
//! * a **scope**: the format's natural unit of regeneration inside one
//!   repository (RPM: the metadata root, `""` at depth zero; conda: the
//!   subdir).
//!
//! A request computes the fingerprint, serves the cached set when it matches
//! (warm path: no catalog query, no render), and otherwise renders once under
//! a per-`(repository, scope)` single-flight lock: concurrent misses for one
//! state coalesce behind the leader and every waiter re-checks the cache
//! before rendering, so N cold requests cost one render.
//!
//! Staleness model: the fingerprint is revalidated on every request, so a
//! change is visible on the very next read in every replica. A caller must
//! capture the fingerprint *before* it reads the rows it renders: a write
//! racing a render can then only make the entry look older than its content,
//! and the next request re-renders. Over-invalidation is possible; serving
//! stale bytes as fresh is not.
//!
//! Bounds: entry count and total byte budget are capped, oldest render
//! first (the newest entry always survives, so one over-budget repository
//! still gets warm requests); each `(repository, scope)` keeps at most
//! [`RevisionCacheConfig::max_partitions_per_scope`] partitions, so one
//! virtual with many visibility classes evicts its own oldest class, not
//! other repositories. Single-flight locks are a fixed set of stripes, so
//! lock memory stays bounded even for a stream of made-up scopes.
//!
//! Metrics (shared labels, so every format that adopts the cache reports the
//! same way):
//!
//! * `ak_repo_metadata_regenerations_total{format,scope}`: full renders;
//! * `ak_repo_metadata_regeneration_seconds{format}`: render duration;
//! * `ak_repo_metadata_cache_requests_total{format,result}`, `result` one of
//!   `hit` (served from the cache), `coalesced` (waited on another request's
//!   render of the same state) or `miss` (rendered).
//!
//! `scope` is the scope *kind* configured for the cache (`root`, `subdir`),
//! not the scope value, so the label set stays bounded.

use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

/// The repository state a render was built from.
pub trait Fingerprint: Clone + Eq + Send + Sync {
    /// The caller-visibility class the render describes. Two callers whose
    /// partitions differ are owed different documents and never share an
    /// entry; callers with the same partition share one render. Formats
    /// whose documents do not depend on the caller return an empty set.
    fn partition(&self) -> Vec<Uuid> {
        Vec::new()
    }
}

/// One immutable rendered document set.
pub trait CachedRender: Send + Sync {
    /// Bytes this set holds, charged against the cache's byte budget.
    fn weight_bytes(&self) -> usize;

    /// False when the render is known to be incomplete for a transient
    /// reason: it is kept only for [`RevisionCacheConfig::incomplete_ttl`],
    /// so a burst of requests shares it and a later request retries.
    fn cacheable(&self) -> bool {
        true
    }

    /// When set, the render is not served after this instant even though
    /// the fingerprint has not moved (a format-specific retry deadline).
    fn valid_until(&self) -> Option<Instant> {
        None
    }
}

/// Limits and labels for one cache instance.
#[derive(Clone, Debug)]
pub struct RevisionCacheConfig {
    /// `format` label on the shared metrics (`rpm`, `conda`).
    pub format: &'static str,
    /// `scope` label on the shared metrics: the kind of unit a scope names
    /// (`root`, `subdir`).
    pub scope_kind: &'static str,
    /// Soft cap on cached `(repository, scope, partition)` entries.
    pub max_entries: usize,
    /// Total byte budget across all entries.
    pub max_bytes: usize,
    /// Most partitions kept for one `(repository, scope)`.
    pub max_partitions_per_scope: usize,
    /// Number of single-flight lock stripes.
    pub lock_stripes: usize,
    /// How long a non-cacheable render is shared before the next request
    /// renders again.
    pub incomplete_ttl: Duration,
}

/// Cache key: repository id, scope, and the render's partition.
pub type CacheKey = (Uuid, String, Vec<Uuid>);

pub(crate) struct CacheEntry<F, V> {
    pub(crate) fingerprint: F,
    pub(crate) rendered: Arc<V>,
    pub(crate) rendered_at: Instant,
}

/// Fingerprint-validated, single-flight cache of rendered metadata sets.
pub struct RepoMetadataRevisionCache<F, V> {
    pub(crate) entries: RwLock<HashMap<CacheKey, CacheEntry<F, V>>>,
    /// Fixed stripes bound lock memory. A `(repository, scope)` always
    /// selects the same stripe whatever the partition; collisions only
    /// serialize.
    pub(crate) render_locks: Vec<Mutex<()>>,
    /// Full renders performed since construction. Observability and the
    /// test hook that proves warm requests do not rebuild.
    renders: AtomicU64,
    config: RevisionCacheConfig,
}

/// How a request was answered, for the `result` metric label.
#[derive(Clone, Copy)]
enum Outcome {
    Hit,
    Coalesced,
    Miss,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Hit => "hit",
            Outcome::Coalesced => "coalesced",
            Outcome::Miss => "miss",
        }
    }
}

impl<F: Fingerprint, V: CachedRender> RepoMetadataRevisionCache<F, V> {
    pub fn with_config(config: RevisionCacheConfig) -> Self {
        let config = RevisionCacheConfig {
            max_entries: config.max_entries.max(1),
            max_partitions_per_scope: config.max_partitions_per_scope.max(1),
            lock_stripes: config.lock_stripes.max(1),
            ..config
        };
        Self {
            entries: RwLock::new(HashMap::new()),
            render_locks: (0..config.lock_stripes).map(|_| Mutex::new(())).collect(),
            renders: AtomicU64::new(0),
            config,
        }
    }

    pub fn config(&self) -> &RevisionCacheConfig {
        &self.config
    }

    /// Total full renders performed since construction.
    pub fn renders(&self) -> u64 {
        self.renders.load(Ordering::Relaxed)
    }

    /// Drop every cached partition of `(repo_id, scope)`. For a consumer
    /// told by another replica that the scope moved: the fingerprint check
    /// would refuse the entry anyway, this frees its memory now.
    pub async fn invalidate_scope(&self, repo_id: Uuid, scope: &str) {
        self.entries
            .write()
            .await
            .retain(|key, _| !(key.0 == repo_id && key.1 == scope));
    }

    /// Drop every cached entry of `repo_id`, whatever its scope.
    pub async fn invalidate_repo(&self, repo_id: Uuid) {
        self.entries.write().await.retain(|key, _| key.0 != repo_id);
    }

    /// The cached set for `(repo_id, scope)`, but only when it was rendered
    /// from exactly the state `fingerprint` describes and is still fresh.
    pub async fn lookup_scoped(
        &self,
        repo_id: Uuid,
        scope: &str,
        fingerprint: &F,
    ) -> Option<Arc<V>> {
        let entries = self.entries.read().await;
        let entry = entries.get(&(repo_id, scope.to_owned(), fingerprint.partition()))?;
        let fresh = (entry.rendered.cacheable()
            || entry.rendered_at.elapsed() < self.config.incomplete_ttl)
            && entry
                .rendered
                .valid_until()
                .is_none_or(|t| Instant::now() < t);
        if entry.fingerprint == *fingerprint && fresh {
            Some(entry.rendered.clone())
        } else {
            None
        }
    }

    /// Serve the set for `(repo_id, scope, fingerprint)`, rendering at most
    /// once per state change. A render failure is returned to the caller and
    /// caches nothing, so the next request retries.
    pub async fn get_or_render_scoped<E, R, Fut>(
        &self,
        repo_id: Uuid,
        scope: &str,
        fingerprint: F,
        render: R,
    ) -> Result<Arc<V>, E>
    where
        R: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        if let Some(hit) = self.lookup_scoped(repo_id, scope, &fingerprint).await {
            self.record(Outcome::Hit);
            return Ok(hit);
        }
        let key = (repo_id, scope.to_owned(), fingerprint.partition());
        let lock = self.render_lock(repo_id, scope);
        let _guard = lock.lock().await;
        // Re-check: the leader that held the lock may have rendered exactly
        // this state while we waited.
        if let Some(hit) = self.lookup_scoped(repo_id, scope, &fingerprint).await {
            self.record(Outcome::Coalesced);
            return Ok(hit);
        }
        let started = Instant::now();
        let rendered = Arc::new(render().await?);
        self.renders.fetch_add(1, Ordering::Relaxed);
        self.record(Outcome::Miss);
        metrics::counter!(
            "ak_repo_metadata_regenerations_total",
            "format" => self.config.format,
            "scope" => self.config.scope_kind,
        )
        .increment(1);
        metrics::histogram!(
            "ak_repo_metadata_regeneration_seconds",
            "format" => self.config.format,
        )
        .record(started.elapsed().as_secs_f64());
        self.insert(key, fingerprint, rendered.clone()).await;
        Ok(rendered)
    }

    fn record(&self, outcome: Outcome) {
        metrics::counter!(
            "ak_repo_metadata_cache_requests_total",
            "format" => self.config.format,
            "result" => outcome.label(),
        )
        .increment(1);
    }

    /// The single-flight lock for a `(repository, scope)`. Deliberately NOT
    /// keyed by the partition (#4346): renders of different partitions of
    /// one scope serialize, so a caller who can vary its visibility (for
    /// example by minting repository-scoped tokens) cannot drive concurrent
    /// full renders of one repository.
    fn render_lock(&self, repo_id: Uuid, scope: &str) -> &Mutex<()> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (repo_id, scope).hash(&mut hasher);
        &self.render_locks[hasher.finish() as usize % self.render_locks.len()]
    }

    async fn insert(&self, key: CacheKey, fingerprint: F, rendered: Arc<V>) {
        let mut entries = self.entries.write().await;
        // Per-scope partition cap (#4346): evict the oldest other partition
        // of this repository/scope before adding a new one.
        loop {
            let same_scope: Vec<(&CacheKey, &CacheEntry<F, V>)> = entries
                .iter()
                .filter(|(k, _)| k.0 == key.0 && k.1 == key.1 && k.2 != key.2)
                .collect();
            if same_scope.len() < self.config.max_partitions_per_scope {
                break;
            }
            let Some(oldest) = same_scope
                .iter()
                .max_by_key(|(_, e)| e.rendered_at.elapsed())
                .map(|(k, _)| (*k).clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            CacheEntry {
                fingerprint,
                rendered,
                rendered_at: Instant::now(),
            },
        );
        // Enforce the entry cap and the byte budget, oldest render first. The
        // just-inserted entry is the newest, so it survives unless it is the
        // only one left: a single over-budget repository still gets cached.
        loop {
            let over_entries = entries.len() > self.config.max_entries;
            let total: usize = entries.values().map(|e| e.rendered.weight_bytes()).sum();
            let over_bytes = total > self.config.max_bytes && entries.len() > 1;
            if !over_entries && !over_bytes {
                break;
            }
            let Some(oldest) = entries
                .iter()
                .max_by_key(|(_, e)| e.rendered_at.elapsed())
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
    }
}

#[cfg(ak_test_shard = "services-2")]
#[cfg(test)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
    use std::sync::atomic::AtomicUsize;

    /// A conda-shaped consumer: a fingerprint with no caller partition and
    /// a scope per subdir. Proves the cache is not tied to RPM types.
    #[derive(Clone, PartialEq, Eq, Debug)]
    struct SubdirState(u64);
    impl Fingerprint for SubdirState {}

    #[derive(Debug)]
    struct Doc(Vec<u8>);
    impl CachedRender for Doc {
        fn weight_bytes(&self) -> usize {
            self.0.len()
        }
    }

    fn cache() -> RepoMetadataRevisionCache<SubdirState, Doc> {
        RepoMetadataRevisionCache::with_config(RevisionCacheConfig {
            format: "testfmt",
            scope_kind: "subdir",
            max_entries: 8,
            max_bytes: usize::MAX,
            max_partitions_per_scope: 4,
            lock_stripes: 8,
            incomplete_ttl: Duration::from_secs(60),
        })
    }

    /// Counter values keyed by `name` and the sorted label pairs. Read once:
    /// a `DebuggingRecorder` snapshot resets counters.
    fn counters(snapshotter: &Snapshotter) -> HashMap<String, u64> {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter_map(|(key, _, _, value)| {
                let DebugValue::Counter(n) = value else {
                    return None;
                };
                let mut labels: Vec<String> = key
                    .key()
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                labels.sort();
                Some((format!("{}{{{}}}", key.key().name(), labels.join(",")), n))
            })
            .collect()
    }

    /// Scopes of one repository render and cache independently, and the
    /// fingerprint of one scope never serves another.
    #[tokio::test]
    async fn scopes_of_one_repository_are_independent_4174() {
        let cache = cache();
        let repo = Uuid::new_v4();
        for subdir in ["linux-64", "noarch"] {
            let out = cache
                .get_or_render_scoped::<(), _, _>(repo, subdir, SubdirState(1), || async move {
                    Ok(Doc(subdir.as_bytes().to_vec()))
                })
                .await
                .unwrap();
            assert_eq!(out.0, subdir.as_bytes());
        }
        assert_eq!(cache.renders(), 2);
        let hit = cache
            .lookup_scoped(repo, "noarch", &SubdirState(1))
            .await
            .unwrap();
        assert_eq!(hit.0, b"noarch");
        assert!(cache
            .lookup_scoped(repo, "noarch", &SubdirState(2))
            .await
            .is_none());
    }

    /// The shared metrics: a burst of cold requests for one state records
    /// exactly one regeneration and one miss; the rest are coalesced, and a
    /// later request is a hit (#4174 acceptance: one regeneration per state
    /// change).
    #[tokio::test]
    async fn burst_of_cold_requests_records_one_regeneration_4174() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let cache = Arc::new(cache());
        let repo = Uuid::new_v4();
        let calls = Arc::new(AtomicUsize::new(0));
        // A current-thread runtime: spawned tasks see the thread-local
        // recorder installed above.
        let tasks: Vec<_> = (0..24)
            .map(|_| {
                let cache = cache.clone();
                let calls = calls.clone();
                tokio::spawn(async move {
                    cache
                        .get_or_render_scoped::<(), _, _>(
                            repo,
                            "linux-64",
                            SubdirState(7),
                            || async move {
                                calls.fetch_add(1, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                Ok(Doc(b"repodata".to_vec()))
                            },
                        )
                        .await
                        .unwrap()
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        cache
            .get_or_render_scoped::<(), _, _>(repo, "linux-64", SubdirState(7), || async {
                Ok(Doc(b"never".to_vec()))
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let c = counters(&snapshotter);
        assert_eq!(
            c.get("ak_repo_metadata_regenerations_total{format=testfmt,scope=subdir}"),
            Some(&1),
            "{c:?}"
        );
        assert_eq!(
            c.get("ak_repo_metadata_cache_requests_total{format=testfmt,result=miss}"),
            Some(&1),
            "{c:?}"
        );
        assert_eq!(
            c.get("ak_repo_metadata_cache_requests_total{format=testfmt,result=coalesced}"),
            Some(&23),
            "{c:?}"
        );
        assert_eq!(
            c.get("ak_repo_metadata_cache_requests_total{format=testfmt,result=hit}"),
            Some(&1),
            "{c:?}"
        );
    }

    /// A failed render records no regeneration and caches nothing.
    #[tokio::test]
    async fn failed_render_records_no_regeneration_4174() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let cache = cache();
        let repo = Uuid::new_v4();
        let err = cache
            .get_or_render_scoped::<&str, _, _>(repo, "noarch", SubdirState(1), || async {
                Err("db down")
            })
            .await
            .unwrap_err();
        assert_eq!(err, "db down");
        assert_eq!(cache.renders(), 0);
        let c = counters(&snapshotter);
        assert!(
            !c.keys()
                .any(|k| k.starts_with("ak_repo_metadata_regenerations_total")),
            "{c:?}"
        );
        assert!(cache
            .lookup_scoped(repo, "noarch", &SubdirState(1))
            .await
            .is_none());
    }
}

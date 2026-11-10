//! Per-Remote proxy-cache byte cap (#4660).
//!
//! A Remote repository's cache used to grow until the `max_age_days` /
//! `no_downloads_days` lifecycle windows expired: `quota_bytes` only refused an
//! object larger than itself (#4551) and lifecycle `size_quota_bytes` is
//! rejected on a Remote (#3734). This module gives a Remote a cap on the
//! bytes its proxy cache holds.
//!
//! # Where the cap lives
//!
//! `repository_config` key [`CAP_CONFIG_KEY`], next to the other per-Remote
//! proxy settings (`cache_ttl_secs`, `stale_if_error_grace_secs`,
//! `on_upstream_not_found`), set through
//! `PUT /api/v1/repositories/{key}/proxy-cache-cap`. No schema change, and
//! `quota_bytes` keeps its meaning (a per-object ceiling on a Remote).
//!
//! # What happens at the cap
//!
//! * **Admission.** An object larger than the cap is never cached: it is
//!   streamed to the client with no cache write, exactly like an object over
//!   `quota_bytes`.
//! * **Eviction (the default).** After every cache commit the repository's
//!   usage (the `repository_usage_ledger.proxy_bytes` counter the catalogue
//!   triggers keep exact) is compared with the cap, and the least recently
//!   used entries (the later of the last serve and the last write) are evicted
//!   until usage is back at or under it. Eviction deletes the entry's objects
//!   and then its catalogue row, the order the lifecycle sweep uses, and keeps
//!   the entry's download history (#4540). Runs are debounced per repository,
//!   so a burst of fills costs one usage read and one LRU scan a second, not
//!   one per fill.
//! * **Refusal (the fallback).** When eviction cannot get under the cap
//!   (storage refuses the deletes, or nothing evictable is left) new fills are
//!   refused, served to the client without a cache write, until a later run
//!   gets under the cap again. A full cache therefore never grows past its cap
//!   because eviction is failing.
//!
//! Usage can exceed the cap briefly: by the fill that just committed, until
//! its eviction run finishes, and by fills that start before a cap is lowered.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::services::proxy_cache_scope::ProxyCacheScope;
use crate::services::storage_service::StorageService;

/// `repository_config` key holding the cap in bytes (a decimal string).
pub const CAP_CONFIG_KEY: &str = "proxy_cache_cap_bytes";

/// How long a replica trusts its memo of a repository's cap. A change made on
/// another replica applies within this window; one made here, at once.
const CAP_MEMO_TTL: Duration = Duration::from_secs(30);

/// How long a repository whose eviction could not get under the cap refuses
/// new fills before the next run is allowed to change that.
const REFUSAL_TTL: Duration = Duration::from_secs(60);

/// Minimum spacing between two eviction runs for one repository.
const ENFORCEMENT_DEBOUNCE: Duration = Duration::from_secs(1);

/// Candidates fetched per page of an eviction run.
const EVICTION_PAGE_SIZE: i64 = 50;

/// Consecutive storage failures that end an eviction run.
const MAX_EVICTION_FAILURES: usize = 10;

/// What a Remote does when its cache reaches the cap. Only one behaviour
/// exists today; it is reported so a client can tell.
pub const AT_CAP_BEHAVIOUR: &str = "evict_lru";

/// The stored cap, `None` when unset or not a positive integer (unset means
/// no cap, as before).
pub fn cap_from_config_value(value: Option<&str>) -> Option<i64> {
    value
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|bytes| *bytes > 0)
}

/// Read the cap of `repository_id` from `repository_config`.
pub async fn load_cap(db: &PgPool, repository_id: Uuid) -> Result<Option<i64>> {
    let raw: Option<String> = sqlx::query_scalar(
        "SELECT value FROM repository_config WHERE repository_id = $1 AND key = $2",
    )
    .bind(repository_id)
    .bind(CAP_CONFIG_KEY)
    .fetch_optional(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(cap_from_config_value(raw.as_deref()))
}

/// Store (`Some`) or clear (`None`) the cap of `repository_id`.
pub async fn save_cap(db: &PgPool, repository_id: Uuid, cap_bytes: Option<i64>) -> Result<()> {
    match cap_bytes {
        Some(bytes) => {
            if bytes <= 0 {
                return Err(AppError::Validation(
                    "cap_bytes must be a positive number of bytes; DELETE the cap to remove it"
                        .to_string(),
                ));
            }
            sqlx::query(
                "INSERT INTO repository_config (repository_id, key, value) VALUES ($1, $2, $3) \
                 ON CONFLICT (repository_id, key) DO UPDATE SET value = EXCLUDED.value, \
                 updated_at = NOW()",
            )
            .bind(repository_id)
            .bind(CAP_CONFIG_KEY)
            .bind(bytes.to_string())
            .execute(db)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        }
        None => {
            sqlx::query("DELETE FROM repository_config WHERE repository_id = $1 AND key = $2")
                .bind(repository_id)
                .bind(CAP_CONFIG_KEY)
                .execute(db)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
        }
    }
    Ok(())
}

/// Bytes and entries a Remote repository's proxy cache holds now. Bytes come
/// from the usage ledger (exact through the catalogue triggers, migration
/// 182), falling back to the live sum for a repository with no ledger row.
pub async fn usage(db: &PgPool, repository_id: Uuid) -> Result<CacheUsage> {
    let (bytes, entries): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE( \
             (SELECT proxy_bytes FROM repository_usage_ledger WHERE repository_id = $1), \
             (SELECT SUM(size_bytes) FROM proxy_cache_artifacts WHERE repository_id = $1), \
             0)::BIGINT, \
         (SELECT COUNT(*) FROM proxy_cache_artifacts \
           WHERE repository_id = $1 AND size_bytes > 0)::BIGINT",
    )
    .bind(repository_id)
    .fetch_one(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(CacheUsage { bytes, entries })
}

/// The bytes half of [`usage`], without the entry count: what an eviction
/// run compares with the cap after every commit and every page, where a
/// `COUNT(*)` over a large cache would be wasted work.
async fn usage_bytes(db: &PgPool, repository_id: Uuid) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT COALESCE( \
             (SELECT proxy_bytes FROM repository_usage_ledger WHERE repository_id = $1), \
             (SELECT SUM(size_bytes) FROM proxy_cache_artifacts WHERE repository_id = $1), \
             0)::BIGINT",
    )
    .bind(repository_id)
    .fetch_one(db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))
}

/// Current proxy-cache usage of one repository.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CacheUsage {
    /// Bytes held by the repository's cached objects.
    pub bytes: i64,
    /// Cached objects (catalogue rows with a body).
    pub entries: i64,
}

/// What one eviction run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapEnforcement {
    pub usage_before: i64,
    pub usage_after: i64,
    pub entries_evicted: i64,
    pub bytes_freed: i64,
    /// Entries kept because deleting their objects failed.
    pub failures: usize,
}

#[derive(Debug, sqlx::FromRow)]
struct LruCandidate {
    id: Uuid,
    path: String,
    storage_key: String,
    metadata_key: String,
    size_bytes: i64,
    cached_at: DateTime<Utc>,
}

/// Least-recently-used cache entries of one repository, oldest first. `$3`
/// lists ids already tried and kept, so a stuck entry is not re-selected.
/// Placeholder rows (no body yet) hold no bytes and are never candidates.
const LRU_CANDIDATES_SQL: &str = "SELECT id, path, storage_key, metadata_key, size_bytes, \
     cached_at FROM proxy_cache_artifacts \
     WHERE repository_id = $1 AND size_bytes > 0 AND NOT (id = ANY($3)) \
     ORDER BY GREATEST(cached_at, COALESCE(last_accessed_at, cached_at)), id \
     LIMIT $2";

/// Per-repository eviction run state: whether another commit arrived while a
/// run was in progress.
#[derive(Debug, Default)]
struct RunState {
    dirty: bool,
}

/// Enforces the cap for every Remote repository this process caches for.
/// Shared by the cache writers (a commit schedules a run) and the API (setting
/// a cap runs one at once).
pub struct ProxyCacheCap {
    db: PgPool,
    storage: Arc<StorageService>,
    scope: ProxyCacheScope,
    caps: Cache<Uuid, Option<i64>>,
    refusing: Cache<Uuid, ()>,
    runs: std::sync::Mutex<HashMap<Uuid, RunState>>,
}

impl ProxyCacheCap {
    pub fn new(db: PgPool, storage: Arc<StorageService>, scope: ProxyCacheScope) -> Arc<Self> {
        Arc::new(Self {
            db,
            storage,
            scope,
            caps: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(CAP_MEMO_TTL)
                .build(),
            refusing: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(REFUSAL_TTL)
                .build(),
            runs: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// The cap in force for `repository_id`, memoized for [`CAP_MEMO_TTL`]. A
    /// lookup failure is not memoized and reads as "no cap".
    pub async fn cap_bytes(&self, repository_id: Uuid) -> Option<i64> {
        if let Some(hit) = self.caps.get(&repository_id).await {
            return hit;
        }
        match load_cap(&self.db, repository_id).await {
            Ok(cap) => {
                self.caps.insert(repository_id, cap).await;
                cap
            }
            Err(e) => {
                tracing::warn!(
                    repository_id = %repository_id,
                    error = %e,
                    "proxy-cache cap: could not read the cap; treating the repository as uncapped"
                );
                None
            }
        }
    }

    /// Forget this process's memo of `repository_id`'s cap and refusal state,
    /// so a change made through the API applies to the next fill here.
    pub async fn invalidate(&self, repository_id: Uuid) {
        self.caps.invalidate(&repository_id).await;
        self.refusing.invalidate(&repository_id).await;
    }

    /// Whether `repository_id` takes new cache fills: `false` only while the
    /// last eviction run could not get under the cap (the refusal fallback).
    pub async fn admits_fills(&self, repository_id: Uuid) -> bool {
        !self.refusing.contains_key(&repository_id)
    }

    /// A fill of `repository_id` just committed: schedule an eviction run.
    /// Returns at once; the run happens on a spawned task, at most one at a
    /// time per repository, and a commit arriving during a run makes it run
    /// once more after [`ENFORCEMENT_DEBOUNCE`].
    pub fn note_fill(self: &Arc<Self>, repository_id: Uuid) {
        {
            let Ok(mut runs) = self.runs.lock() else {
                return;
            };
            if let Some(state) = runs.get_mut(&repository_id) {
                state.dirty = true;
                return;
            }
            runs.insert(repository_id, RunState::default());
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if let Some(cap) = this.cap_bytes(repository_id).await {
                    if let Err(e) = this.enforce(repository_id, cap).await {
                        tracing::warn!(
                            repository_id = %repository_id,
                            error = %e,
                            "proxy-cache cap: eviction run failed"
                        );
                    }
                }
                let again = match this.runs.lock() {
                    Ok(mut runs) => match runs.get_mut(&repository_id) {
                        Some(state) if state.dirty => {
                            state.dirty = false;
                            true
                        }
                        _ => {
                            runs.remove(&repository_id);
                            false
                        }
                    },
                    Err(_) => false,
                };
                if !again {
                    break;
                }
                tokio::time::sleep(ENFORCEMENT_DEBOUNCE).await;
            }
        });
    }

    /// Evict least-recently-used entries of `repository_id` until its cache
    /// holds at most `cap_bytes`, and set or clear the refusal fallback from
    /// the outcome.
    ///
    /// Each entry's objects are deleted before its catalogue row, and the row
    /// delete is guarded on `cached_at` so an entry re-cached meanwhile is
    /// kept. Usage is re-read after every page, so fills and evictions on
    /// other replicas are accounted for; two replicas running at once can
    /// overshoot by at most a page, which only costs refetches.
    pub async fn enforce(&self, repository_id: Uuid, cap_bytes: i64) -> Result<CapEnforcement> {
        let mut current = usage_bytes(&self.db, repository_id).await?;
        let mut report = CapEnforcement {
            usage_before: current,
            usage_after: current,
            ..CapEnforcement::default()
        };
        if current <= cap_bytes {
            self.refusing.invalidate(&repository_id).await;
            return Ok(report);
        }
        let repository_key: Option<String> =
            sqlx::query_scalar("SELECT key FROM repositories WHERE id = $1")
                .bind(repository_id)
                .fetch_optional(&self.db)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
        let Some(repository_key) = repository_key else {
            return Ok(report);
        };
        let mut kept: Vec<Uuid> = Vec::new();
        let mut consecutive_failures = 0usize;
        'pages: while current > cap_bytes {
            let page: Vec<LruCandidate> = sqlx::query_as(LRU_CANDIDATES_SQL)
                .bind(repository_id)
                .bind(EVICTION_PAGE_SIZE)
                .bind(&kept)
                .fetch_all(&self.db)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            if page.is_empty() {
                break;
            }
            for entry in page {
                if current <= cap_bytes {
                    break;
                }
                let objects = crate::services::proxy_service::evict_entry_objects(
                    &self.storage,
                    &self.scope,
                    &repository_key,
                    &entry.path,
                    [entry.storage_key.as_str(), entry.metadata_key.as_str()],
                )
                .await;
                if let Err(e) = objects {
                    tracing::warn!(
                        repository = %repository_key,
                        path = %entry.path,
                        error = %e,
                        "proxy-cache cap: could not evict entry; keeping it"
                    );
                    kept.push(entry.id);
                    report.failures += 1;
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_EVICTION_FAILURES {
                        break 'pages;
                    }
                    continue;
                }
                consecutive_failures = 0;
                let deleted = sqlx::query(
                    "DELETE FROM proxy_cache_artifacts WHERE id = $1 AND cached_at = $2",
                )
                .bind(entry.id)
                .bind(entry.cached_at)
                .execute(&self.db)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?
                .rows_affected();
                if deleted > 0 {
                    report.entries_evicted += 1;
                    report.bytes_freed += entry.size_bytes;
                    current -= entry.size_bytes;
                } else {
                    // Re-cached while its objects were deleted: the row now
                    // describes new bytes. Leave it, and do not pick it again.
                    kept.push(entry.id);
                }
            }
            current = usage_bytes(&self.db, repository_id).await?;
        }
        report.usage_after = current;
        if current > cap_bytes {
            self.refusing.insert(repository_id, ()).await;
            crate::services::metrics_service::record_proxy_cache_cap_refusing(&repository_key);
            tracing::warn!(
                repository = %repository_key,
                usage_bytes = current,
                cap_bytes,
                failures = report.failures,
                "proxy-cache cap: eviction could not get the cache under its cap; refusing new \
                 cache fills (served without caching) until it can"
            );
        } else {
            self.refusing.invalidate(&repository_id).await;
        }
        if report.entries_evicted > 0 {
            crate::services::metrics_service::record_proxy_cache_cap_evictions(
                &repository_key,
                report.entries_evicted as u64,
                report.bytes_freed.max(0) as u64,
            );
            tracing::info!(
                repository = %repository_key,
                entries = report.entries_evicted,
                bytes = report.bytes_freed,
                usage_bytes = current,
                cap_bytes,
                "proxy-cache cap: evicted least recently used entries"
            );
        }
        Ok(report)
    }
}

#[cfg(ak_test_shard = "services-1")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_value_maps_to_a_positive_cap() {
        assert_eq!(cap_from_config_value(Some("5242880")), Some(5_242_880));
        assert_eq!(cap_from_config_value(Some(" 1 ")), Some(1));
        assert_eq!(cap_from_config_value(Some("0")), None);
        assert_eq!(cap_from_config_value(Some("-1")), None);
        assert_eq!(cap_from_config_value(Some("5 MiB")), None);
        assert_eq!(cap_from_config_value(None), None);
    }

    use crate::api::handlers::test_db_helpers as tdh;
    use crate::services::storage_service::FilesystemBackend;

    const MIB: i64 = 1024 * 1024;

    fn cap_for(pool: &PgPool, dir: &std::path::Path) -> Arc<ProxyCacheCap> {
        ProxyCacheCap::new(
            pool.clone(),
            Arc::new(StorageService::new(Arc::new(FilesystemBackend::new(
                dir.to_path_buf(),
            )))),
            ProxyCacheScope::unscoped(),
        )
    }

    /// Seed one catalogue row of `size` bytes last served `idle_mins` ago,
    /// recorded under `key_root` (this repository's cache, or somewhere else).
    async fn seed_entry(pool: &PgPool, repo: Uuid, key_root: &str, n: usize, idle_mins: i32) {
        let path = format!("pkgs/obj-{n}.bin");
        sqlx::query(
            "INSERT INTO proxy_cache_artifacts (repository_id, path, storage_key, metadata_key, \
             size_bytes, checksum_sha256, cached_at, last_accessed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, NOW() - INTERVAL '1 day', \
             NOW() - make_interval(mins => $7))",
        )
        .bind(repo)
        .bind(&path)
        .bind(format!("{key_root}/{path}/__content__"))
        .bind(format!("{key_root}/{path}/__cache_meta__.json"))
        .bind(MIB)
        .bind(format!("{:0>64}", n))
        .bind(idle_mins)
        .execute(pool)
        .await
        .expect("seed cache entry");
    }

    async fn paths(pool: &PgPool, repo: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT path FROM proxy_cache_artifacts WHERE repository_id = $1 ORDER BY path",
        )
        .bind(repo)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// #4660: ten 1 MiB entries under a 5 MiB cap: the five least recently
    /// served are evicted, the five most recent stay, and usage ends at the
    /// cap.
    #[tokio::test]
    async fn eviction_removes_least_recently_used_entries_until_under_cap() {
        let Some(pool) = tdh::try_pool().await else {
            return;
        };
        let (repo, key, dir) = tdh::create_repo(&pool, "remote", "generic").await;
        // obj-0 is the most recently served, obj-9 the least.
        for n in 0..10 {
            seed_entry(
                &pool,
                repo,
                &format!("proxy-cache/{key}"),
                n,
                (n as i32 + 1) * 10,
            )
            .await;
        }
        let cap = cap_for(&pool, &dir);
        let report = cap.enforce(repo, 5 * MIB).await.expect("enforce");
        let left = paths(&pool, repo).await;
        let admits = cap.admits_fills(repo).await;
        let _ = sqlx::query("DELETE FROM repositories WHERE id = $1")
            .bind(repo)
            .execute(&pool)
            .await;

        assert_eq!(report.usage_before, 10 * MIB);
        assert_eq!(report.usage_after, 5 * MIB);
        assert_eq!(report.entries_evicted, 5);
        assert_eq!(
            left,
            (0..5)
                .map(|n| format!("pkgs/obj-{n}.bin"))
                .collect::<Vec<_>>(),
            "the five most recently served entries stay"
        );
        assert!(admits, "a cache under its cap takes fills");
    }

    /// #4660 refusal fallback: entries whose objects cannot be deleted are
    /// kept, the cache stays over its cap, and new fills are refused until a
    /// run gets under it.
    #[tokio::test]
    async fn failing_eviction_refuses_new_fills() {
        let Some(pool) = tdh::try_pool().await else {
            return;
        };
        let (repo, _key, dir) = tdh::create_repo(&pool, "remote", "generic").await;
        // Recorded outside this repository's cache root: eviction refuses to
        // delete them (a renamed repository's leftovers), every one a failure.
        for n in 0..3 {
            seed_entry(&pool, repo, "proxy-cache/elsewhere", n, 10).await;
        }
        let cap = cap_for(&pool, &dir);
        let report = cap.enforce(repo, MIB).await.expect("enforce");
        let refused = !cap.admits_fills(repo).await;
        // Lift the cap above usage: the next run clears the refusal.
        cap.enforce(repo, 10 * MIB).await.expect("enforce");
        let admits_again = cap.admits_fills(repo).await;
        let left = paths(&pool, repo).await.len();
        let _ = sqlx::query("DELETE FROM repositories WHERE id = $1")
            .bind(repo)
            .execute(&pool)
            .await;

        assert_eq!(report.entries_evicted, 0);
        assert_eq!(report.failures, 3);
        assert_eq!(report.usage_after, 3 * MIB);
        assert_eq!(
            left, 3,
            "nothing it could not delete is dropped from the catalogue"
        );
        assert!(
            refused,
            "over the cap with eviction failing, fills are refused"
        );
        assert!(admits_again);
    }

    /// #4660 refusal fallback, end to end: while eviction cannot get the
    /// cache under its cap, a proxied fetch still answers the client with the
    /// full upstream body (buffered and streamed), writes nothing to the
    /// cache, and is not counted as an over-quota object.
    #[test]
    fn refused_fills_are_still_served_to_the_client() {
        use futures::StreamExt;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let (outcome, counters) =
            crate::services::metrics_service::capture_counters_async(|| async {
                let pool = tdh::try_pool().await?;
                let body = vec![9u8; 4096];
                let mock = MockServer::start().await;
                Mock::given(method("GET"))
                    .and(path_regex(r"^/pkgs/.*"))
                    .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
                    .mount(&mock)
                    .await;
                let (repo_id, key, dir) = tdh::create_repo(&pool, "remote", "generic").await;
                sqlx::query("UPDATE repositories SET upstream_url = $1 WHERE id = $2")
                    .bind(mock.uri())
                    .bind(repo_id)
                    .execute(&pool)
                    .await
                    .unwrap();
                tdh::publish_repo(&pool, repo_id).await;
                // Over a 1 MiB cap with entries eviction refuses to delete.
                for n in 0..3 {
                    seed_entry(&pool, repo_id, "proxy-cache/elsewhere", n, 10).await;
                }
                save_cap(&pool, repo_id, Some(MIB)).await.unwrap();

                let proxy = tdh::build_proxy_service_with_fs(pool.clone(), dir.to_str().unwrap());
                proxy
                    .cache_cap()
                    .enforce(repo_id, MIB)
                    .await
                    .expect("enforce");
                let refusing = !proxy.cache_cap().admits_fills(repo_id).await;
                let repo = crate::api::handlers::proxy_helpers::build_remote_repo_with_format(
                    repo_id,
                    &key,
                    &mock.uri(),
                    crate::models::repository::RepositoryFormat::Generic,
                );

                let (buffered, _) = proxy
                    .fetch_artifact(&repo, "pkgs/buffered.bin")
                    .await
                    .expect("a refused buffered fill still serves the client");
                let mut streamed = proxy
                    .fetch_artifact_streaming_with_cache_path(
                        &repo,
                        "pkgs/streamed.bin",
                        "pkgs/streamed.bin",
                    )
                    .await
                    .expect("a refused streamed fill still serves the client");
                let mut streamed_len = 0usize;
                while let Some(chunk) = streamed.body.next().await {
                    streamed_len += chunk.expect("chunk").len();
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let cached = [
                    dir.join(format!(
                        "proxy-cache/{key}/pkgs/buffered.bin/__cache_meta__.json"
                    )),
                    dir.join(format!(
                        "proxy-cache/{key}/pkgs/streamed.bin/__cache_meta__.json"
                    )),
                ]
                .iter()
                .any(|p| p.exists());
                let _ = sqlx::query("DELETE FROM repositories WHERE id = $1")
                    .bind(repo_id)
                    .execute(&pool)
                    .await;
                Some((
                    refusing,
                    buffered.len(),
                    streamed_len,
                    body.len(),
                    cached,
                    key,
                ))
            });
        let Some((refusing, buffered_len, streamed_len, body_len, cached, key)) = outcome else {
            return;
        };

        assert!(
            refusing,
            "the setup must leave the repository refusing fills"
        );
        assert_eq!(
            buffered_len, body_len,
            "buffered client gets the whole body"
        );
        assert_eq!(
            streamed_len, body_len,
            "streamed client gets the whole body"
        );
        assert!(!cached, "a refused fill writes no cache entry");
        assert_eq!(
            counters.get(
                "ak_proxy_cache_quota_exceeded_total",
                &[("repository", key.as_str())]
            ),
            0,
            "a refused fill is not an over-quota object"
        );
    }

    /// #4660 end to end: a capped Remote that streams objects through the
    /// proxy ends at or under its cap, the most recent fill kept, and an
    /// object larger than the cap is served without being cached.
    #[tokio::test]
    async fn proxied_fills_stay_under_the_cap() {
        use futures::StreamExt;
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let Some(pool) = tdh::try_pool().await else {
            return;
        };
        let body = vec![7u8; 64 * 1024];
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/small/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/big/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 256 * 1024]))
            .mount(&mock)
            .await;
        let (repo_id, key, dir) = tdh::create_repo(&pool, "remote", "generic").await;
        sqlx::query("UPDATE repositories SET upstream_url = $1 WHERE id = $2")
            .bind(mock.uri())
            .bind(repo_id)
            .execute(&pool)
            .await
            .unwrap();
        tdh::publish_repo(&pool, repo_id).await;
        let cap_bytes = 3 * body.len() as i64;
        save_cap(&pool, repo_id, Some(cap_bytes)).await.unwrap();

        let proxy = tdh::build_proxy_service_with_fs(pool.clone(), dir.to_str().unwrap());
        let repo = crate::api::handlers::proxy_helpers::build_remote_repo_with_format(
            repo_id,
            &key,
            &mock.uri(),
            crate::models::repository::RepositoryFormat::Generic,
        );
        let fetch = |path: String| {
            let proxy = Arc::clone(&proxy);
            let repo = repo.clone();
            async move {
                let mut res = proxy
                    .fetch_artifact_streaming_with_cache_path(&repo, &path, &path)
                    .await
                    .expect("proxied fetch");
                let mut n = 0usize;
                while let Some(chunk) = res.body.next().await {
                    n += chunk.expect("chunk").len();
                }
                n
            }
        };
        for i in 0..6 {
            assert_eq!(fetch(format!("small/obj-{i}.bin")).await, body.len());
            // Let the writer commit before the next fill, so LRU order is
            // the fill order.
            tdh::await_proxy_sidecar(&dir.join(format!(
                "proxy-cache/{key}/small/obj-{i}.bin/__cache_meta__.json"
            )))
            .await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(fetch("big/huge.bin".to_string()).await, 256 * 1024);

        // Eviction runs after the commit, debounced; wait for it to settle.
        let mut used = i64::MAX;
        for _ in 0..100 {
            used = usage(&pool, repo_id).await.unwrap().bytes;
            if used <= cap_bytes {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let left = paths(&pool, repo_id).await;
        let _ = sqlx::query("DELETE FROM repositories WHERE id = $1")
            .bind(repo_id)
            .execute(&pool)
            .await;

        assert!(used <= cap_bytes, "usage {used} over cap {cap_bytes}");
        assert!(
            left.contains(&"small/obj-5.bin".to_string()),
            "the most recent fill is kept: {left:?}"
        );
        assert!(
            !left.contains(&"small/obj-0.bin".to_string()),
            "the least recently used fill is evicted: {left:?}"
        );
        assert!(
            !left.contains(&"big/huge.bin".to_string()),
            "an object larger than the cap is not cached: {left:?}"
        );
    }
}

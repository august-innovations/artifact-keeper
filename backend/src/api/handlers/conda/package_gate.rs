//! Fast-fail database gate on the conda package route (#4182).
//!
//! rattler-based installers fetch up to 50 packages at once each, so a fleet
//! of installers puts far more requests in flight than the pool has
//! connections (`DATABASE_MAX_CONNECTIONS`, 50 by default). A package request
//! that queued on the pool waited up to `DATABASE_ACQUIRE_TIMEOUT_SECS` (5 s)
//! and was then answered 503: the worst signal for a client with its own
//! retry policy, since every queued request stalls before it is told to back
//! off, and the stall itself holds the client's connection slot.
//!
//! The package route's one lookup statement (#4181) takes its connection
//! through [`acquire`] instead: when no connection frees up within
//! `CONDA_PACKAGE_DB_WAIT_MS` (default 500 ms) the request is shed at once
//! with `503 Service Unavailable` and `Retry-After: 1`, counted in
//! `ak_conda_package_requests_shed_total`. A connection that frees up in
//! time is used as before, so the gate only changes requests that would
//! otherwise have queued. `0` turns the gate off (the pool's own acquire
//! timeout applies, as before).

use std::sync::OnceLock;
use std::time::Duration;

use axum::response::{IntoResponse, Response};
use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres};

use crate::error::AppError;

/// Longest a package request waits for a pooled connection before it is
/// shed, in milliseconds. `0` disables the gate.
pub(super) const PACKAGE_DB_WAIT_MS_ENV: &str = "CONDA_PACKAGE_DB_WAIT_MS";

const DEFAULT_WAIT_MS: u64 = 500;

/// Counter of package requests shed by the gate.
pub(super) const SHED_METRIC: &str = "ak_conda_package_requests_shed_total";

/// Parse [`PACKAGE_DB_WAIT_MS_ENV`]: unset, blank or unparseable is the
/// default (logged when it was set), `0` is `None` (gate off).
fn parse_wait(raw: Option<&str>) -> Option<Duration> {
    let ms = match raw.map(str::trim).filter(|v| !v.is_empty()) {
        None => DEFAULT_WAIT_MS,
        Some(value) => value.parse::<u64>().unwrap_or_else(|_| {
            tracing::warn!(
                value,
                default_ms = DEFAULT_WAIT_MS,
                "{PACKAGE_DB_WAIT_MS_ENV} is not a number of milliseconds; using the default"
            );
            DEFAULT_WAIT_MS
        }),
    };
    (ms > 0).then(|| Duration::from_millis(ms))
}

/// The configured wait, read once.
fn configured_wait() -> Option<Duration> {
    static WAIT: OnceLock<Option<Duration>> = OnceLock::new();
    *WAIT.get_or_init(|| {
        let wait = parse_wait(std::env::var(PACKAGE_DB_WAIT_MS_ENV).ok().as_deref());
        tracing::info!(
            wait_ms = wait.map(|w| w.as_millis() as u64).unwrap_or(0),
            "conda package database gate configured"
        );
        wait
    })
}

/// A pooled connection for the package route's lookup, or the shed response.
pub(super) async fn acquire(db: &PgPool) -> Result<PoolConnection<Postgres>, Response> {
    acquire_within(db, configured_wait()).await
}

/// [`acquire`] with an explicit wait (`None`: no gate).
pub(super) async fn acquire_within(
    db: &PgPool,
    wait: Option<Duration>,
) -> Result<PoolConnection<Postgres>, Response> {
    let Some(wait) = wait else {
        return db.acquire().await.map_err(map_acquire_err);
    };
    match tokio::time::timeout(wait, db.acquire()).await {
        Ok(acquired) => acquired.map_err(map_acquire_err),
        Err(_) => {
            metrics::counter!(SHED_METRIC).increment(1);
            tracing::debug!(
                wait_ms = wait.as_millis() as u64,
                pool_size = db.size(),
                "conda package request shed: no database connection within the wait"
            );
            Err(shed_response())
        }
    }
}

/// A pool error keeps its usual classification (a pool timeout is
/// `503 POOL_EXHAUSTED`).
fn map_acquire_err(e: sqlx::Error) -> Response {
    AppError::Sqlx(e).into_response()
}

/// `503` with `Retry-After: 1` (added by `AppError` for every 503).
fn shed_response() -> Response {
    AppError::ServiceUnavailable(
        "The database connection pool is saturated; retry shortly".to_string(),
    )
    .into_response()
}

#[cfg(ak_test_shard = "handlers-1")]
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, StatusCode};

    #[test]
    fn parse_wait_defaults_and_zero_disables() {
        let default = Some(Duration::from_millis(DEFAULT_WAIT_MS));
        assert_eq!(parse_wait(None), default);
        assert_eq!(parse_wait(Some("")), default);
        assert_eq!(parse_wait(Some(" 250 ")), Some(Duration::from_millis(250)));
        assert_eq!(parse_wait(Some("0")), None);
        assert_eq!(parse_wait(Some("soon")), default);
        assert_eq!(parse_wait(Some("-1")), default);
    }

    #[test]
    fn shed_response_is_a_retryable_503() {
        let response = shed_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    }
}

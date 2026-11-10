//! Scheduled `audit_log` retention (#4718).
//!
//! The `audit_retention_days` system setting (`POST /api/v1/admin/settings`)
//! used to be applied only when an administrator called
//! `POST /api/v1/admin/cleanup` with `cleanup_audit_logs`, so an operator who
//! configured a retention period still got unbounded growth. An hourly
//! scheduler pass now applies it:
//!
//! - The setting is read on every pass, so a change takes effect on the next
//!   hour without a restart. A missing, zero, negative or non-numeric value
//!   disables the pass: nothing is ever deleted unless a positive period was
//!   explicitly stored.
//! - Rows with `created_at` older than the period are deleted oldest first in
//!   batches of [`AUDIT_RETENTION_BATCH_SIZE`], one short statement each, so a
//!   large backlog never holds one long transaction or lock.
//! - One replica runs a pass at a time (scheduler lease, renewed for the whole
//!   pass); a lost lease stops the pass between batches.
//! - Each pass logs one summary line; the period in force is logged once at
//!   startup.
//!
//! The manual cleanup endpoint is unchanged.

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, Result};

/// The `system_settings` key holding the retention period in days.
pub const AUDIT_RETENTION_SETTING: &str = "audit_retention_days";
/// Scheduler lease name; one replica runs a pass at a time.
pub const AUDIT_RETENTION_LEASE_NAME: &str = "audit_retention";
/// Lease TTL, renewed for as long as a pass runs.
pub const AUDIT_RETENTION_LEASE_TTL_SECS: f64 = 600.0;
/// How often the pass runs.
pub const AUDIT_RETENTION_INTERVAL_SECS: u64 = 3600;
/// Rows deleted per statement.
pub const AUDIT_RETENTION_BATCH_SIZE: i64 = 5000;

/// The retention period a stored setting value configures: `Some(days)` for a
/// positive integer, `None` (retention off) for anything else, including an
/// absent setting.
pub fn retention_days_from_setting(value: Option<&serde_json::Value>) -> Option<i32> {
    value
        .and_then(serde_json::Value::as_i64)
        .filter(|days| *days > 0)
        .map(|days| i32::try_from(days).unwrap_or(i32::MAX))
}

/// Read the configured retention period (`None` when retention is off).
pub async fn configured_retention_days(db: &PgPool) -> Result<Option<i32>> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM system_settings WHERE key = $1")
            .bind(AUDIT_RETENTION_SETTING)
            .fetch_optional(db)
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(retention_days_from_setting(value.as_ref()))
}

/// The outcome of one retention pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AuditRetentionReport {
    /// The period applied; `None` when retention is off and nothing ran.
    pub retention_days: Option<i32>,
    pub rows_deleted: u64,
    pub batches: u32,
    /// The pass stopped early because the scheduler lease was lost.
    pub interrupted: bool,
}

/// Delete `audit_log` rows older than `retention_days` (when `Some` and
/// positive) in batches of `batch_size`, oldest first, until none are left or
/// `lost` is cancelled.
pub async fn apply_retention(
    db: &PgPool,
    retention_days: Option<i32>,
    batch_size: i64,
    lost: Option<&CancellationToken>,
) -> Result<AuditRetentionReport> {
    let mut report = AuditRetentionReport {
        retention_days: retention_days.filter(|days| *days > 0),
        ..Default::default()
    };
    let Some(days) = report.retention_days else {
        return Ok(report);
    };
    let batch_size = batch_size.max(1);
    loop {
        if lost.is_some_and(CancellationToken::is_cancelled) {
            report.interrupted = true;
            break;
        }
        let deleted = sqlx::query(
            "DELETE FROM audit_log WHERE id IN ( \
                 SELECT id FROM audit_log \
                 WHERE created_at < NOW() - make_interval(days => $1) \
                 ORDER BY created_at \
                 LIMIT $2)",
        )
        .bind(days)
        .bind(batch_size)
        .execute(db)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?
        .rows_affected();
        report.rows_deleted += deleted;
        report.batches += 1;
        if deleted < batch_size as u64 {
            break;
        }
    }
    Ok(report)
}

/// The startup line stating the retention period in force.
pub fn startup_message(retention_days: Option<i32>) -> String {
    match retention_days {
        Some(days) => format!(
            "Audit retention: audit_log rows older than {days} days are deleted every hour \
             (system setting {AUDIT_RETENTION_SETTING})"
        ),
        None => format!(
            "Audit retention is off: system setting {AUDIT_RETENTION_SETTING} is not set to a \
             positive number of days, so audit_log rows are kept until removed manually"
        ),
    }
}

#[cfg(ak_test_shard = "services-1")]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::handlers::test_db_helpers as tdh;
    use uuid::Uuid;

    #[test]
    fn setting_value_maps_to_retention_period() {
        let days = |v: serde_json::Value| retention_days_from_setting(Some(&v));
        assert_eq!(days(serde_json::json!(90)), Some(90));
        assert_eq!(days(serde_json::json!(1)), Some(1));
        assert_eq!(days(serde_json::json!(0)), None);
        assert_eq!(days(serde_json::json!(-5)), None);
        assert_eq!(days(serde_json::json!("90")), None);
        assert_eq!(days(serde_json::json!(null)), None);
        assert_eq!(retention_days_from_setting(None), None);
        assert_eq!(days(serde_json::json!(i64::MAX)), Some(i32::MAX));
    }

    #[test]
    fn startup_message_states_the_period_or_that_retention_is_off() {
        assert!(startup_message(Some(30)).contains("older than 30 days"));
        assert!(startup_message(None).contains("off"));
    }

    /// Seed one `audit_log` row `age_days` old under a marker resource id.
    async fn seed_row(pool: &PgPool, marker: Uuid, age_days: i32) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO audit_log (user_id, action, resource_type, resource_id, correlation_id, \
             created_at) VALUES (NULL, 'SETTING_CHANGED', 'setting', $1, $2, \
             NOW() - make_interval(days => $3)) RETURNING id",
        )
        .bind(marker)
        .bind(Uuid::new_v4().to_string())
        .bind(age_days)
        .fetch_one(pool)
        .await
        .expect("seed audit row")
    }

    async fn surviving(pool: &PgPool, ids: &[Uuid]) -> Vec<Uuid> {
        let mut rows: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM audit_log WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await
            .expect("surviving rows");
        rows.sort();
        rows
    }

    /// The pass deletes every row older than the period, in batches, and keeps
    /// every row inside it.
    #[tokio::test]
    async fn deletes_rows_past_the_period_and_keeps_the_rest() {
        let Some(pool) = tdh::try_pool().await else {
            return;
        };
        let marker = Uuid::new_v4();
        let old_a = seed_row(&pool, marker, 40).await;
        let old_b = seed_row(&pool, marker, 31).await;
        let recent = seed_row(&pool, marker, 29).await;
        let fresh = seed_row(&pool, marker, 0).await;

        // A batch of one forces several statements for the two old rows.
        let report = apply_retention(&pool, Some(30), 1, None)
            .await
            .expect("retention pass");
        let left = surviving(&pool, &[old_a, old_b, recent, fresh]).await;
        let _ = sqlx::query("DELETE FROM audit_log WHERE resource_id = $1")
            .bind(marker)
            .execute(&pool)
            .await;

        let mut kept = vec![recent, fresh];
        kept.sort();
        assert_eq!(left, kept, "only rows inside the period may survive");
        assert_eq!(report.retention_days, Some(30));
        assert!(report.rows_deleted >= 2, "{report:?}");
        assert!(report.batches >= 3, "batched one row at a time: {report:?}");
        assert!(!report.interrupted);
    }

    /// No period (absent or zero setting) deletes nothing; a lost lease stops
    /// the pass before it deletes. The row is 2 days old: a `Some(1)` pass
    /// would delete it, while the 30-day pass the test above runs (possibly
    /// concurrently, the pass is instance-wide) leaves it alone.
    #[tokio::test]
    async fn no_period_or_lost_lease_deletes_nothing() {
        let Some(pool) = tdh::try_pool().await else {
            return;
        };
        let marker = Uuid::new_v4();
        let aged_row = seed_row(&pool, marker, 2).await;
        let absent = apply_retention(&pool, None, 10, None)
            .await
            .expect("absent");
        let zero = apply_retention(&pool, Some(0), 10, None)
            .await
            .expect("zero");
        let lost = CancellationToken::new();
        lost.cancel();
        let interrupted = apply_retention(&pool, Some(1), 10, Some(&lost))
            .await
            .expect("lost lease");
        let left = surviving(&pool, &[aged_row]).await;
        let _ = sqlx::query("DELETE FROM audit_log WHERE resource_id = $1")
            .bind(marker)
            .execute(&pool)
            .await;

        assert_eq!(left, vec![aged_row], "nothing may be deleted");
        assert_eq!(absent, AuditRetentionReport::default());
        assert_eq!(zero, AuditRetentionReport::default());
        assert!(interrupted.interrupted);
        assert_eq!(interrupted.rows_deleted, 0);
    }
}

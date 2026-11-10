-- 300_scan_config_instance_lock.sql
-- #4363: an instance administrator's proxy scan policy for a repository can
-- only be weakened by an instance administrator, mirroring the age-gate
-- policy lock of migration 290.
--
-- A repository administrator may change a repository's scan configuration
-- (`PUT /api/v1/repositories/{key}/security`, `require_repo_admin`). While
-- `proxy_scan_instance_locked` is set, that tier may still TIGHTEN the proxy
-- scan policy, but not weaken it: turn `scan_on_proxy` off, move
-- `proxy_scan_action` towards record_only (fail_closed -> fail_open ->
-- record_only), or loosen the severity gate (opt into a threshold, or raise
-- it). Only an instance administrator's write sets the lock (an instance
-- write locks unless it sends `"instance_locked": false`), and nothing a
-- repository administrator does clears it.
--
-- No backfill, unlike 290: repository administrators have always been allowed
-- to write scan configurations, so no existing row is known to be an instance
-- decision. Existing rows start unlocked; an instance administrator locks one
-- by writing it.
--
-- `scan_configs` holds one row per repository; it is not a hot table. NOT
-- NULL with a constant default is catalogue-only on PostgreSQL 11+.
ALTER TABLE scan_configs
    ADD COLUMN IF NOT EXISTS proxy_scan_instance_locked BOOLEAN NOT NULL DEFAULT false;

-- Down path: rollback is SKIP_MIGRATIONS=true on the older release; 1.11 never
-- reads the column (its PUT ignores the lock, see the 1.12.0 release notes on
-- rolling upgrades), so it can stay. To remove it by hand:
--   ALTER TABLE scan_configs DROP COLUMN IF EXISTS proxy_scan_instance_locked;

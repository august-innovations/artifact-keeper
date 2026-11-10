-- 295_proxy_download_statistics_checksum.sql
-- #4542: record the digest that was served on every proxy download row, so the
-- admin blast-radius report can still attribute a pull to a digest after the
-- cache entry it was served from is evicted or purged (a remediation purge of
-- the compromised object is exactly when that question is asked).
--
-- Before this the report joined `proxy_download_statistics` to the live
-- `proxy_cache_artifacts` row by `proxy_cache_id`; since 279 an evicted entry
-- leaves its rows behind with `proxy_cache_id = NULL`, and those rows carried
-- no digest of their own.
--
-- The application writes the column: a cache hit records the digest of the
-- catalog row it was served from, and a cache fill fills the rows of the
-- serves it fed once the digest is known. Existing rows are not backfilled:
-- while their cache entry lives the report still reaches them through
-- `proxy_cache_id`, and once it is gone there is nothing to backfill from.
-- Pods still on 1.11 during a rolling upgrade leave the column NULL, which is
-- the same as an existing row.
--
-- Cost on a million-row table: catalogue-only. A nullable column with no
-- default takes ACCESS EXCLUSIVE for a catalogue update, bounded by
-- lock_timeout; the index is built online by 297.
SET LOCAL lock_timeout = '5s';

ALTER TABLE proxy_download_statistics
    ADD COLUMN IF NOT EXISTS checksum_sha256 TEXT;

-- Down path: rollback is SKIP_MIGRATIONS=true on the older release (see
-- docs/operations/rollback.md); 1.11 never reads or writes this column, so it
-- can stay. To remove it by hand, drop idx_proxy_dl_stats_checksum first and
-- then the column.

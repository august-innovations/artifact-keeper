-- no-transaction
-- 297_proxy_download_statistics_checksum_index.sql
-- #4542: the admin blast-radius report finds the proxy pulls of a CVE's
-- digests by `checksum_sha256`, so the lookup does not scan every proxy
-- download row. Partial: rows recorded before the column existed (and serves
-- of objects that were never cached) carry no digest and are reached through
-- `proxy_cache_id` instead.
--
-- Cost on a million-row table: two scans of `proxy_download_statistics` under
-- SHARE UPDATE EXCLUSIVE, which blocks no reads or writes; the build first
-- waits for transactions already open on the table. An interrupted build is
-- dropped and retried at the next boot (registered in
-- `migration_repair::CONCURRENT_INDEX_MIGRATIONS`). Down path: drop the index
-- by name; nothing in 1.11 uses it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_proxy_dl_stats_checksum
    ON proxy_download_statistics (checksum_sha256, downloaded_at)
    WHERE checksum_sha256 IS NOT NULL;

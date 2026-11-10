-- 296_proxy_download_statistics_checksum_index_reset.sql
-- Drop any leftover idx_proxy_dl_stats_checksum before 297 builds it, the
-- reset half of the docs/operations/online-migrations.md "Index build" pair. It
-- runs once; a retry of an interrupted build is handled at every boot by
-- `migration_repair::repair_invalid_concurrent_indexes` (the build is
-- registered in `CONCURRENT_INDEX_MIGRATIONS`).
DROP INDEX IF EXISTS idx_proxy_dl_stats_checksum;

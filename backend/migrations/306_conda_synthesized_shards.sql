-- #4178: CEP-16 shards synthesised for Remote conda repositories whose
-- upstream publishes no shard index.
--
-- The shards themselves go into `conda_shard_index` (migration 298) with
-- origin = 'synthesized', and the served index revision into
-- `conda_subdir_revisions`, exactly as a hosted channel's. This table records,
-- per (repository, subdir), which cached upstream document they were built
-- from, so a refresh can tell whether the upstream index changed:
--
--   source_path    the upstream document read (`repodata.json.zst`, `.bz2`
--                  or `repodata.json`, relative to the remote's upstream)
--   source_sha256  SHA-256 hex of its bytes as the proxy cache holds them
--                  (the proxy cache entry's own checksum, so freshness is a
--                  metadata comparison, no read of the document)
--   built_at       when the stored shards last matched that document
--   stale_since    set when a refresh found a different upstream document
--                  and cleared by the rebuild; a stale index is not served
--   checked_at     last time the upstream document was compared
--
-- Additive only. 1.11.x replicas never read it (their remote shard index
-- keeps answering 404 for an upstream without one).
--
-- Rollback:
--   DELETE FROM conda_shard_index WHERE origin = 'synthesized';
--   DROP TABLE IF EXISTS conda_synthesized_subdirs;

CREATE TABLE IF NOT EXISTS conda_synthesized_subdirs (
    repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    subdir TEXT NOT NULL,
    source_path TEXT,
    source_sha256 TEXT,
    built_at TIMESTAMPTZ,
    stale_since TIMESTAMPTZ,
    checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (repository_id, subdir)
);

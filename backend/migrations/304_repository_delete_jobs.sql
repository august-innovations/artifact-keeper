-- 304_repository_delete_jobs.sql
-- #4311: `DELETE /api/v1/repositories/{key}` records a job here and answers
-- 202; a background worker deletes the repository's rows in bounded batches
-- and reclaims its storage, under a row claim (claim_token / claim_expires_at)
-- so exactly one replica runs a job at a time and a job whose replica died is
-- resumed by another. `GET /api/v1/repository-deletions/{id}` reads it.
--
-- `repository_id` deliberately has no foreign key: the job deletes that row
-- and must outlive it. `repository_key` is the key the repository had when
-- the delete was requested (the request renames the live row so the key is
-- free at once).
--
-- Additive only: 1.11 never reads the table.
CREATE TABLE IF NOT EXISTS repository_delete_jobs (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id     UUID NOT NULL,
    repository_key    TEXT NOT NULL,
    storage_backend   TEXT NOT NULL,
    storage_path      TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'completed', 'failed')),
    phase             TEXT NOT NULL DEFAULT 'queued',
    total_items       INTEGER NOT NULL DEFAULT 0,
    completed_items   INTEGER NOT NULL DEFAULT 0,
    failed_items      INTEGER NOT NULL DEFAULT 0,
    skipped_items     INTEGER NOT NULL DEFAULT 0,
    total_bytes       BIGINT NOT NULL DEFAULT 0,
    deleted_bytes     BIGINT NOT NULL DEFAULT 0,
    oci_temp_keys     JSONB NOT NULL DEFAULT '[]'::jsonb,
    error_summary     TEXT,
    attempts          INTEGER NOT NULL DEFAULT 0,
    claimed_by        TEXT,
    claim_token       UUID,
    claim_expires_at  TIMESTAMPTZ,
    created_by        UUID,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at        TIMESTAMPTZ,
    finished_at       TIMESTAMPTZ,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- One live job per repository: a repeated DELETE returns the job in flight.
CREATE UNIQUE INDEX IF NOT EXISTS idx_repository_delete_jobs_active
    ON repository_delete_jobs (repository_id)
    WHERE status IN ('pending', 'running');

-- The resumer's scan for pending work and lapsed claims.
CREATE INDEX IF NOT EXISTS idx_repository_delete_jobs_claimable
    ON repository_delete_jobs (status, claim_expires_at)
    WHERE status IN ('pending', 'running');

COMMENT ON TABLE repository_delete_jobs IS
    'Repository deletions run as background jobs (#4311). The row outlives the repository it names.';

-- Down path: rollback is SKIP_MIGRATIONS=true on the older release, which
-- never reads the table. A job still pending or running when an older release
-- takes over leaves its repository under the renamed key `~deleting~<job id>`;
-- delete that repository again from the older release. To remove the table by
-- hand:
--   DROP TABLE IF EXISTS repository_delete_jobs;

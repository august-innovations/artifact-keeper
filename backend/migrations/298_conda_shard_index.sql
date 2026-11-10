-- #4173 / #4174: write-time CEP-16 shards and per-subdir repodata revisions
-- for hosted conda channels.
--
-- Before this, every shard request listed every artifact of the repository,
-- rebuilt and compressed shard after shard until one hashed to the requested
-- address, and every repodata document rebuilt the whole subdir per request.
-- These tables hold what those walks produced, so the read path is a lookup:
--
--   conda_shard_index       one row per (repository, subdir, package name):
--                           the encoded `.msgpack.zst` shard and its SHA-256
--                           (the address CEP-16 clients request). A row
--                           exists while the name has a live (not deleted)
--                           row; `shard` is NULL when none of them is visible
--                           (all withdrawn), and the row still records that
--                           the channel owns the name, which the virtual
--                           dependency confusion guard reads. `origin` is 'hosted' for
--                           shards built from the repository's own rows;
--                           'synthesized' is reserved for shards a Remote
--                           repository builds from its cached upstream index
--                           (#4178), which go through the same table.
--   conda_subdir_revisions  a counter per (repository, subdir), bumped every
--                           time the subdir's visible state is rebuilt. It is
--                           the fingerprint of the repodata revision cache.
--   conda_shard_dirty       (repository, subdir, name) keys whose shard must
--                           be rebuilt before the subdir is served again.
--   conda_channel_state     per-repository backfill cursor, and the earliest
--                           future `quarantine_until` of a held row, so a
--                           hold that expires (which changes visibility with
--                           no write) is noticed on the next read.
--
-- Visibility changes reach the dirty set in the SAME transaction as the
-- catalog change, whatever code path makes it: the triggers below fire on
-- every insert, delete and relevant update of `artifacts` and
-- `artifact_metadata` rows of hosted conda repositories (uploads,
-- withdrawals, soft and hard deletes, quarantine transitions from the
-- scanner or the admin console, metadata edits such as attestation
-- sidecars). Readers rebuild any dirty key before they serve, so a stored
-- shard or revision is never served after the catalog moved past it.
--
-- Additive only. 1.11.x replicas never read these tables; their writes still
-- fire the triggers during a rolling upgrade, so the dirty set stays
-- complete. Repositories that already hold conda packages are seeded with a
-- NULL `backfill_done_at`: the 1.12 backfill job fills their shard rows in
-- bounded, resumable batches, and the shard endpoints keep the old
-- per-request walk for a repository until it completes.
--
-- Rollback:
--   DROP TRIGGER IF EXISTS conda_artifacts_mark_dirty_t ON artifacts;
--   DROP TRIGGER IF EXISTS conda_artifacts_mark_dirty_del_t ON artifacts;
--   DROP TRIGGER IF EXISTS conda_artifact_metadata_mark_dirty_t ON artifact_metadata;
--   DROP FUNCTION IF EXISTS conda_artifacts_mark_dirty();
--   DROP FUNCTION IF EXISTS conda_artifact_metadata_mark_dirty();
--   DROP FUNCTION IF EXISTS conda_mark_artifact_dirty(UUID, UUID, TEXT, TEXT);
--   DROP TABLE IF EXISTS conda_shard_dirty, conda_shard_index,
--     conda_subdir_revisions, conda_channel_state;
--   DROP SEQUENCE IF EXISTS conda_shard_dirty_mark_seq;

CREATE TABLE IF NOT EXISTS conda_shard_index (
    repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    subdir TEXT NOT NULL,
    package_name TEXT NOT NULL,
    shard_hash BYTEA,
    shard BYTEA,
    origin TEXT NOT NULL DEFAULT 'hosted'
        CHECK (origin IN ('hosted', 'synthesized')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (repository_id, subdir, package_name),
    CHECK ((shard_hash IS NULL) = (shard IS NULL))
);

CREATE INDEX IF NOT EXISTS conda_shard_index_address
    ON conda_shard_index (repository_id, subdir, shard_hash)
    WHERE shard_hash IS NOT NULL;

CREATE TABLE IF NOT EXISTS conda_subdir_revisions (
    repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    subdir TEXT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (repository_id, subdir)
);

CREATE SEQUENCE IF NOT EXISTS conda_shard_dirty_mark_seq;

CREATE TABLE IF NOT EXISTS conda_shard_dirty (
    repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    subdir TEXT NOT NULL,
    package_name TEXT NOT NULL,
    -- Bumped on every re-mark, so a rebuild deletes only the marks it saw.
    mark BIGINT NOT NULL,
    marked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (repository_id, subdir, package_name)
);

CREATE TABLE IF NOT EXISTS conda_channel_state (
    repository_id UUID PRIMARY KEY REFERENCES repositories(id) ON DELETE CASCADE,
    -- Last package name (lower-cased) the backfill finished; NULL before the
    -- first batch.
    backfill_cursor TEXT,
    -- NULL while the backfill has not finished. A repository with no row
    -- needs no backfill: it was created after this migration, so every one
    -- of its writes went through the triggers.
    backfill_done_at TIMESTAMPTZ,
    next_hold_expiry TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Mark the shard keys one artifact row contributes to. A row's key is its
-- metadata's subdir and name when the metadata says, else the first path
-- segment and the row's name (`artifacts_for_subdir` and
-- `group_artifacts_by_name` in handlers/conda.rs). Both are marked: the
-- metadata row is written after the artifact row on upload.
CREATE OR REPLACE FUNCTION conda_mark_artifact_dirty(
    p_repository_id UUID,
    p_artifact_id UUID,
    p_path TEXT,
    p_name TEXT
) RETURNS VOID
LANGUAGE plpgsql
AS $$
DECLARE
    v_hosted_conda BOOLEAN;
    v_meta JSONB;
    v_path_subdir TEXT;
    v_meta_subdir TEXT;
    v_meta_name TEXT;
BEGIN
    SELECT r.format::text IN ('conda', 'conda_native')
           AND r.repo_type::text IN ('local', 'staging')
      INTO v_hosted_conda
      FROM repositories r
     WHERE r.id = p_repository_id;
    IF v_hosted_conda IS NOT TRUE THEN
        RETURN;
    END IF;

    IF position('/' IN p_path) > 0 THEN
        v_path_subdir := split_part(p_path, '/', 1);
    END IF;
    SELECT am.metadata INTO v_meta
      FROM artifact_metadata am
     WHERE am.artifact_id = p_artifact_id;
    v_meta_subdir := COALESCE(v_meta ->> 'subdir', v_path_subdir);
    v_meta_name := COALESCE(v_meta ->> 'name', p_name);

    IF v_path_subdir IS NOT NULL AND p_name IS NOT NULL THEN
        INSERT INTO conda_shard_dirty (repository_id, subdir, package_name, mark)
        VALUES (p_repository_id, v_path_subdir, p_name, nextval('conda_shard_dirty_mark_seq'))
        ON CONFLICT (repository_id, subdir, package_name)
        DO UPDATE SET mark = EXCLUDED.mark, marked_at = NOW();
    END IF;
    IF v_meta_subdir IS NOT NULL AND v_meta_name IS NOT NULL
       AND (v_meta_subdir IS DISTINCT FROM v_path_subdir OR v_meta_name IS DISTINCT FROM p_name) THEN
        INSERT INTO conda_shard_dirty (repository_id, subdir, package_name, mark)
        VALUES (p_repository_id, v_meta_subdir, v_meta_name, nextval('conda_shard_dirty_mark_seq'))
        ON CONFLICT (repository_id, subdir, package_name)
        DO UPDATE SET mark = EXCLUDED.mark, marked_at = NOW();
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION conda_artifacts_mark_dirty() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM conda_mark_artifact_dirty(OLD.repository_id, OLD.id, OLD.path, OLD.name);
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM conda_mark_artifact_dirty(NEW.repository_id, NEW.id, NEW.path, NEW.name);
        RETURN NEW;
    END IF;
    RETURN OLD;
END;
$$;

CREATE OR REPLACE FUNCTION conda_artifact_metadata_mark_dirty() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    v_artifact RECORD;
    v_old_subdir TEXT;
    v_old_name TEXT;
BEGIN
    SELECT a.id, a.repository_id, a.path, a.name INTO v_artifact
      FROM artifacts a
     WHERE a.id = COALESCE(NEW.artifact_id, OLD.artifact_id);
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    -- The key the row had under its previous metadata (a rename or subdir
    -- edit moves the record out of that shard).
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        v_old_subdir := COALESCE(OLD.metadata ->> 'subdir',
            CASE WHEN position('/' IN v_artifact.path) > 0
                 THEN split_part(v_artifact.path, '/', 1) END);
        v_old_name := COALESCE(OLD.metadata ->> 'name', v_artifact.name);
        IF v_old_subdir IS NOT NULL AND v_old_name IS NOT NULL
           AND EXISTS (SELECT 1 FROM repositories r
                        WHERE r.id = v_artifact.repository_id
                          AND r.format::text IN ('conda', 'conda_native')
                          AND r.repo_type::text IN ('local', 'staging')) THEN
            INSERT INTO conda_shard_dirty (repository_id, subdir, package_name, mark)
            VALUES (v_artifact.repository_id, v_old_subdir, v_old_name,
                    nextval('conda_shard_dirty_mark_seq'))
            ON CONFLICT (repository_id, subdir, package_name)
            DO UPDATE SET mark = EXCLUDED.mark, marked_at = NOW();
        END IF;
    END IF;
    PERFORM conda_mark_artifact_dirty(
        v_artifact.repository_id, v_artifact.id, v_artifact.path, v_artifact.name);
    RETURN NULL;
END;
$$;

-- The triggers. CREATE TRIGGER takes SHARE ROW EXCLUSIVE on `artifacts`
-- and `artifact_metadata`, which queues behind any open transaction and holds
-- every write behind it while it waits (#4153): each attempt waits at most
-- 5 s for its locks, and a lock timeout is retried (5 attempts, backing off)
-- rather than failing the deploy.
--
-- On `artifacts`: insert, and the columns a repodata record or its
-- visibility reads. Deletes run BEFORE, while the metadata row (which
-- cascades) is still readable. On `artifact_metadata`: every change of the
-- document (name, subdir, attestation sidecar).
SET LOCAL lock_timeout = '5s';
DO $$
DECLARE
    attempt INT := 0;
BEGIN
    LOOP
        BEGIN
            SET LOCAL lock_timeout = '5s';
            DROP TRIGGER IF EXISTS conda_artifacts_mark_dirty_t ON artifacts;
            CREATE TRIGGER conda_artifacts_mark_dirty_t
                AFTER INSERT OR UPDATE OF repository_id, path, name, version, size_bytes,
                    checksum_sha256, is_deleted, quarantine_status, quarantine_until, created_at
                ON artifacts
                FOR EACH ROW
                EXECUTE FUNCTION conda_artifacts_mark_dirty();

            DROP TRIGGER IF EXISTS conda_artifacts_mark_dirty_del_t ON artifacts;
            CREATE TRIGGER conda_artifacts_mark_dirty_del_t
                BEFORE DELETE ON artifacts
                FOR EACH ROW
                EXECUTE FUNCTION conda_artifacts_mark_dirty();

            DROP TRIGGER IF EXISTS conda_artifact_metadata_mark_dirty_t ON artifact_metadata;
            CREATE TRIGGER conda_artifact_metadata_mark_dirty_t
                AFTER INSERT OR UPDATE OF metadata OR DELETE ON artifact_metadata
                FOR EACH ROW
                EXECUTE FUNCTION conda_artifact_metadata_mark_dirty();
            EXIT;
        EXCEPTION WHEN lock_not_available THEN
            attempt := attempt + 1;
            IF attempt >= 5 THEN
                RAISE;
            END IF;
            PERFORM pg_sleep(attempt);
        END;
    END LOOP;
END
$$;

-- Seed the backfill for every hosted conda repository that exists now, with
-- its earliest pending hold expiry.
INSERT INTO conda_channel_state (repository_id, backfill_done_at, next_hold_expiry)
SELECT r.id,
       NULL,
       (SELECT MIN(a.quarantine_until)
          FROM artifacts a
         WHERE a.repository_id = r.id
           AND a.is_deleted = false
           AND a.quarantine_status = 'quarantined'
           AND a.quarantine_until > NOW())
  FROM repositories r
 WHERE r.format::text IN ('conda', 'conda_native')
   AND r.repo_type::text IN ('local', 'staging')
ON CONFLICT (repository_id) DO NOTHING;

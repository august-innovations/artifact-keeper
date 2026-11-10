-- #4483 / #4545: stop charging OCI manifest rows to `hosted_bytes`.
--
-- A pushed (or proxy-cached, migrated-and-reindexed) OCI manifest's
-- `artifacts` row lives under an `oci-manifests/<digest>` storage key and
-- records the IMAGE size, `config.size + layers[].size` (#3601). Those config
-- and layer bytes are already charged to `oci_bytes` through `oci_blobs`, so
-- the trigger from migration 182 counted every image at least twice, and once
-- more for every extra tag of the same image: a repository holding 1.10 GB of
-- blobs reported 4.69 GB used, and storage quotas on Docker repositories
-- tripped at a fraction of the real footprint. The storage-stats refresher
-- already excludes these rows for the same reason (#3286).
--
-- This file:
--   1. replaces `ak_usage_ledger_artifacts()` with one that counts an
--      `artifacts` row iff `is_deleted = false`, its key is not
--      `proxy-cache/%` AND not `oci-manifests/%`. The trigger itself is
--      unchanged (it already fires on `storage_key` updates and calls the
--      function by name), so no trigger DDL runs and no table lock is taken
--      on `artifacts`;
--   2. re-seeds `hosted_bytes` of every repository that holds an
--      `oci-manifests/%` row with the absolute recompute under the new rule
--      (the same SUM `RepositoryService::reconcile_usage_ledger` and the
--      admin `backfill_usage_ledgers` endpoint now use). Other repositories
--      and the `proxy_bytes` / `oci_bytes` components are untouched.
--
-- `artifacts.size_bytes` keeps meaning the image size: the per-tag listing,
-- the packages catalog and the UI read it. Only the usage figures change.
--
-- Cost on a million-row `artifacts` table: CREATE OR REPLACE FUNCTION is a
-- catalogue update. The re-seed reads, per affected repository, its live
-- `artifacts` rows through the `repository_id` index (the affected set is
-- found with one `EXISTS` probe per ledger row) and updates one small
-- `repository_usage_ledger` row each. No `artifacts` row is written. One
-- transaction, so the new rule and the corrected counters land together; a
-- write that commits while this runs and was not in its snapshot is repaired
-- by the background reconciler, as for migration 182's true-up.
SET LOCAL lock_timeout = '5s';

CREATE OR REPLACE FUNCTION ak_usage_ledger_artifacts() RETURNS trigger AS $$
DECLARE
    old_contrib BIGINT := 0;
    new_contrib BIGINT := 0;
BEGIN
    IF TG_OP = 'UPDATE' OR TG_OP = 'DELETE' THEN
        old_contrib := CASE
            WHEN OLD.is_deleted = false
             AND OLD.storage_key NOT LIKE 'proxy-cache/%'
             AND OLD.storage_key NOT LIKE 'oci-manifests/%'
                THEN COALESCE(OLD.size_bytes, 0)
            ELSE 0
        END;
    END IF;
    IF TG_OP = 'INSERT' OR TG_OP = 'UPDATE' THEN
        new_contrib := CASE
            WHEN NEW.is_deleted = false
             AND NEW.storage_key NOT LIKE 'proxy-cache/%'
             AND NEW.storage_key NOT LIKE 'oci-manifests/%'
                THEN COALESCE(NEW.size_bytes, 0)
            ELSE 0
        END;
    END IF;

    IF TG_OP = 'DELETE' THEN
        PERFORM ak_usage_ledger_apply(OLD.repository_id, -old_contrib, 0, 0);
        RETURN OLD;
    ELSIF TG_OP = 'UPDATE' AND NEW.repository_id IS DISTINCT FROM OLD.repository_id THEN
        PERFORM ak_usage_ledger_apply(OLD.repository_id, -old_contrib, 0, 0);
        PERFORM ak_usage_ledger_apply(NEW.repository_id, new_contrib, 0, 0);
    ELSE
        PERFORM ak_usage_ledger_apply(NEW.repository_id, new_contrib - old_contrib, 0, 0);
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

UPDATE repository_usage_ledger l
   SET hosted_bytes = COALESCE((
           SELECT SUM(a.size_bytes) FROM artifacts a
            WHERE a.repository_id = l.repository_id
              AND a.is_deleted = false
              AND a.storage_key NOT LIKE 'proxy-cache/%'
              AND a.storage_key NOT LIKE 'oci-manifests/%'), 0),
       updated_at = now()
 WHERE EXISTS (
           SELECT 1 FROM artifacts m
            WHERE m.repository_id = l.repository_id
              AND m.storage_key LIKE 'oci-manifests/%');

-- 302_oci_gc_candidates_leased_at.sql
-- #4980: record when a storage GC pass took an `oci_gc_candidates` row as a
-- lease for a delete it is about to run, so the upload commit guard can tell
-- an in-flight delete from a candidate that is only waiting out its grace
-- window.
--
-- `recorded_at` serves both purposes today. A repository delete records its
-- spared keys with `recorded_at = NOW()` and the candidate sweep takes them
-- only once they are older than `BLOB_GC_MIN_AGE_SECS`; GC's orphan-key claim
-- journals a key it is deleting with `recorded_at = NOW()`, and the candidate
-- sweep leases a row by moving `recorded_at` to now. A fresh `recorded_at`
-- therefore does not say whether a delete is in flight, and an upload that
-- treated every fresh row as one would be refused for a day after the
-- repository that held the same content was deleted.
--
-- `leased_at` is maintained by a trigger so that every writer, including a
-- 1.11 replica during a rolling upgrade, keeps it right without code changes:
--   * an INSERT with no `source_repository_id` is GC's own journal entry for a
--     delete in flight: `leased_at = recorded_at`;
--   * an UPDATE that moves `recorded_at` is a lease (the candidate sweep's
--     claim, or the orphan-key claim taking over an existing row):
--     `leased_at = recorded_at`;
--   * a repository-delete recording (with `source_repository_id`) and an
--     UPDATE that leaves `recorded_at` alone keep `leased_at` as it was.
--
-- Additive only: 1.11 never reads the column. Existing rows start with NULL
-- (no lease known), which is the state a waiting candidate has.
ALTER TABLE oci_gc_candidates
    ADD COLUMN IF NOT EXISTS leased_at TIMESTAMPTZ;

CREATE OR REPLACE FUNCTION oci_gc_candidates_track_lease() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.source_repository_id IS NULL THEN
            NEW.leased_at := NEW.recorded_at;
        END IF;
    ELSIF NEW.recorded_at IS DISTINCT FROM OLD.recorded_at THEN
        NEW.leased_at := NEW.recorded_at;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS oci_gc_candidates_track_lease ON oci_gc_candidates;
CREATE TRIGGER oci_gc_candidates_track_lease
    BEFORE INSERT OR UPDATE ON oci_gc_candidates
    FOR EACH ROW EXECUTE FUNCTION oci_gc_candidates_track_lease();

COMMENT ON COLUMN oci_gc_candidates.leased_at IS
    'When a storage GC pass last took this row as a lease for a delete it runs next (#4980). NULL for a candidate only waiting out its grace window. Maintained by trigger oci_gc_candidates_track_lease.';

-- Down path: rollback is SKIP_MIGRATIONS=true on the older release, which never
-- reads the column or depends on the trigger. To remove them by hand:
--   DROP TRIGGER IF EXISTS oci_gc_candidates_track_lease ON oci_gc_candidates;
--   DROP FUNCTION IF EXISTS oci_gc_candidates_track_lease();
--   ALTER TABLE oci_gc_candidates DROP COLUMN IF EXISTS leased_at;

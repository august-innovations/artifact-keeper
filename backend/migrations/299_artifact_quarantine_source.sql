-- 299_artifact_quarantine_source.sql
-- #4510: record where a hosted artifact's hold came from, so the download gate
-- can answer a scan-policy hold with the scan-policy 403 instead of the
-- quarantine 409 without matching on the free-text `quarantine_reason` (an
-- administrator can type "Policy '...'" too), and the admin quarantine queue
-- can label and filter holds by origin.
--
-- Values: 'upload' (the upload-time quarantine period, always timed), 'admin'
-- (quarantine-now), 'scan_policy' (a scan policy refused the artifact after a
-- scan). NULL means "not recorded": every hold written before this
-- migration, every hold a 1.11 replica writes during a rolling upgrade, and
-- the scanner's own policy holds until the scanner records the source. The
-- gate treats NULL by asking the scan policy (the same evaluation its policy
-- half runs), so a NULL-source hold that a policy refuses already answers the
-- 403; nothing has to be backfilled for the gate to be right.
--
-- `quarantine_status` keeps 'quarantined' for every kind of hold, so no filter
-- or guard that reads the status changes. The column sits beside it on
-- `artifacts` (there is no holds table: a hold is the artifact's own stamp, and
-- the gate reads it in the same row it already selects).
--
-- Cost on a million-row table: catalogue-only. A nullable column with no
-- default and a NOT VALID check take ACCESS EXCLUSIVE for a catalogue update,
-- bounded by lock_timeout. The check is NOT VALID because every existing row is
-- NULL and there is nothing to validate; new writes are checked.
SET LOCAL lock_timeout = '5s';

ALTER TABLE artifacts
    ADD COLUMN IF NOT EXISTS quarantine_source TEXT;

ALTER TABLE artifacts
    ADD CONSTRAINT ck_artifacts_quarantine_source
    CHECK (quarantine_source IS NULL
           OR quarantine_source IN ('upload', 'admin', 'scan_policy'))
    NOT VALID;

-- Down path: rollback is SKIP_MIGRATIONS=true on the older release (see
-- docs/operations/rollback.md); 1.11 never reads or writes this column, so it
-- can stay. To remove it by hand:
--   ALTER TABLE artifacts DROP CONSTRAINT IF EXISTS ck_artifacts_quarantine_source;
--   ALTER TABLE artifacts DROP COLUMN IF EXISTS quarantine_source;

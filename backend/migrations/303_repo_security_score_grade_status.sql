-- 303_repo_security_score_grade_status.sql
-- #4336: an explicit grade status and coverage on repository security scores,
-- replacing the F floors of #2167 (failed scan), #4036 and #4154 (archive no
-- scanner could catalog).
--
-- `grade_status` is 'rated' (everything scanned was fully assessed),
-- 'incomplete' (something was assessed and something was not, or only in
-- part) or 'unrated' (nothing was assessed). `rated_grade` is the letter over
-- the assessed artifacts, never floored, NULL when unrated. The *_count
-- columns are the coverage: live artifacts, scanned, cataloged (of which
-- partial), failed and uncataloged, per artifact from the latest scan of each
-- scanner.
--
-- `status_calculated_at` is the `calculated_at` the status and coverage were
-- computed with. A 1.11 replica's recalculation moves `calculated_at` and
-- leaves the new columns as they were, so a mismatch marks them stale.
--
-- Rolling upgrade: additive only. `grade` keeps its meaning and 1.12 keeps
-- writing it exactly as 1.11 does (floored), so a 1.11 replica reads the same
-- letter it always did. For a row a 1.11 replica wrote (new columns NULL or
-- stale), 1.12 derives the status from the legacy flags and the stored letter
-- until its own next recalculation. Nothing is backfilled: every score is
-- recalculated after the next scan of its repository, and the derivation
-- covers the rows in between.
--
-- Cost: catalogue-only. Nullable columns with no default and a NOT VALID
-- check take ACCESS EXCLUSIVE for a catalogue update, bounded by
-- lock_timeout; existing rows are NULL and there is nothing to validate.
SET LOCAL lock_timeout = '5s';

ALTER TABLE repo_security_scores
    ADD COLUMN IF NOT EXISTS grade_status TEXT,
    ADD COLUMN IF NOT EXISTS rated_grade TEXT,
    ADD COLUMN IF NOT EXISTS artifact_count INTEGER,
    ADD COLUMN IF NOT EXISTS scanned_count INTEGER,
    ADD COLUMN IF NOT EXISTS cataloged_count INTEGER,
    ADD COLUMN IF NOT EXISTS partial_count INTEGER,
    ADD COLUMN IF NOT EXISTS failed_count INTEGER,
    ADD COLUMN IF NOT EXISTS uncataloged_count INTEGER,
    ADD COLUMN IF NOT EXISTS status_calculated_at TIMESTAMPTZ;

ALTER TABLE repo_security_scores
    ADD CONSTRAINT ck_repo_security_scores_grade_status
    CHECK (grade_status IS NULL
           OR grade_status IN ('rated', 'incomplete', 'unrated'))
    NOT VALID;

ALTER TABLE repo_security_scores
    ADD CONSTRAINT ck_repo_security_scores_rated_grade
    CHECK (rated_grade IS NULL OR rated_grade IN ('A', 'B', 'C', 'D', 'F'))
    NOT VALID;

-- Down path: rollback is SKIP_MIGRATIONS=true on the older release (see
-- docs/operations/rollback.md); 1.11 never reads or writes these columns, so
-- they can stay. To remove them by hand:
--   ALTER TABLE repo_security_scores
--       DROP CONSTRAINT IF EXISTS ck_repo_security_scores_grade_status,
--       DROP CONSTRAINT IF EXISTS ck_repo_security_scores_rated_grade,
--       DROP COLUMN IF EXISTS grade_status,
--       DROP COLUMN IF EXISTS rated_grade,
--       DROP COLUMN IF EXISTS artifact_count,
--       DROP COLUMN IF EXISTS scanned_count,
--       DROP COLUMN IF EXISTS cataloged_count,
--       DROP COLUMN IF EXISTS partial_count,
--       DROP COLUMN IF EXISTS failed_count,
--       DROP COLUMN IF EXISTS uncataloged_count,
--       DROP COLUMN IF EXISTS status_calculated_at;

-- 301_proxy_scan_results_pin_identity.sql
-- #4952: record the coordinate a proxy verdict was graded as.
--
-- A proxy verdict is stored per content digest. For most formats the bytes
-- name the coordinate they are served as (an npm tarball's package.json, a
-- crate's Cargo.toml), so one verdict per digest is enough. A Composer dist
-- that declares no version, a Conan archive, and a Debian or Alpine package
-- graded against the release its repository names are graded as the REQUEST
-- coordinate (or release), which the bytes do not carry: the same bytes
-- requested under another version, reference or release can grade
-- differently. The inline gate records the coordinate it graded with
-- (`ExpectedComponent::pin_identity`, e.g. `debian|openssl|3.0.11-1|debian:12`)
-- and reuses a verdict for those formats only when it was graded as the
-- coordinate being requested; otherwise the bytes are scanned again.
--
-- NULL means the verdict was graded without a coordinate (the rescan
-- endpoint, a format that pins nothing), or was recorded before this
-- migration or by a 1.11 replica during a rolling upgrade. 1.11 gates none of
-- the formats this applies to, and the gate treats a NULL row as not graded
-- as any coordinate, so nothing has to be backfilled.
--
-- Cost: catalogue-only. A nullable column with no default takes ACCESS
-- EXCLUSIVE for a catalogue update, bounded by lock_timeout.
SET LOCAL lock_timeout = '5s';

ALTER TABLE proxy_scan_results
    ADD COLUMN IF NOT EXISTS pin_identity TEXT;

-- Down path: rollback is SKIP_MIGRATIONS=true on the older release (see
-- docs/operations/rollback.md); 1.11 never reads or writes this column, so it
-- can stay. To remove it by hand:
--   ALTER TABLE proxy_scan_results DROP COLUMN IF EXISTS pin_identity;

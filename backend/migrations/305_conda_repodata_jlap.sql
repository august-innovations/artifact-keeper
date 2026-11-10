-- #4175: JLAP (`repodata.json.jlap`) from stored repodata revisions for
-- hosted conda channels.
--
-- Before this, a hosted subdir's JLAP was a bootstrap file: the hash of the
-- current `repodata.json` and no patches, so every JLAP client paid for the
-- JLAP round trip and then downloaded the whole index anyway. These tables
-- keep what a real JLAP needs:
--
--   conda_repodata_revisions  the last N rendered `repodata.json` documents
--                             of each (repository, subdir), zstd-compressed,
--                             with their BLAKE2b-256 (the hash JLAP clients
--                             compute over the bytes they downloaded). N is
--                             AK_CONDA_JLAP_REVISIONS (default 8). The newest
--                             row is the document the next patch is computed
--                             from; older rows are the documents the patches
--                             kept in the JLAP lead from.
--   conda_repodata_jlap       the stored JLAP of each (repository, subdir):
--                             the IV line, one RFC 6902 patch line per kept
--                             transition, the footer naming the latest hash,
--                             and the trailing checksum of the BLAKE2b chain.
--                             New patch lines are appended in place of the
--                             old footer, so the bytes a client already holds
--                             keep their offsets and a `Range: bytes=<pos>-`
--                             request returns only what is new.
--
-- Both tables are written by the render of a subdir revision, in one
-- transaction under the JLAP row's lock, before that revision's documents are
-- served: the JLAP a replica serves next to `repodata.json` always ends at
-- that document's hash.
--
-- Additive only. 1.11.x replicas never read these tables; they keep serving
-- their bootstrap JLAP during a rolling upgrade.
--
-- Rollback:
--   DROP TABLE IF EXISTS conda_repodata_jlap, conda_repodata_revisions;

CREATE TABLE IF NOT EXISTS conda_repodata_revisions (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    subdir TEXT NOT NULL,
    -- conda_subdir_revisions.revision the document was rendered at.
    revision BIGINT NOT NULL,
    -- BLAKE2b-256 of the uncompressed `repodata.json` bytes as served.
    blake2b BYTEA NOT NULL,
    -- zstd of those bytes (the `repodata.json.zst` served at the revision).
    document BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS conda_repodata_revisions_subdir
    ON conda_repodata_revisions (repository_id, subdir, id DESC);

CREATE TABLE IF NOT EXISTS conda_repodata_jlap (
    repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    subdir TEXT NOT NULL,
    -- The complete JLAP file; NULL until the subdir's first render.
    jlap BYTEA,
    -- Revision and hash of the document the footer names.
    latest_revision BIGINT,
    latest_hash BYTEA,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (repository_id, subdir)
);

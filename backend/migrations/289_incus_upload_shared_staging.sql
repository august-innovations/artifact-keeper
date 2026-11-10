-- Stage Incus chunked-upload chunks on the repository's storage backend (#4302).
--
-- A chunked Incus upload appended every chunk to a file on the local disk of
-- the replica that served the start, so behind more than one backend replica a
-- chunk or the completion routed to another replica found no staged data and
-- the upload failed. Each chunk is now an object on the repository's backend
-- (written through the #3916 generic staging path, so `generic_upload_staging`
-- tracks it and the hourly sweep reclaims an abandoned one), and this column
-- records the session's chunks in commit order:
--
--   [{"key": "<storage key>", "offset": <first byte>, "size": <bytes>}, ...]
--
-- It is appended in the same guarded UPDATE that advances `bytes_received`,
-- so the recorded chunks always cover exactly the committed bytes. The
-- completing replica copies them into its own staging file and finalizes as
-- before.
--
-- NULL is deliberate: it marks a session started by an earlier server version,
-- whose chunks are in its local `storage_temp_path` file. Those sessions keep
-- the local-file path until they finish or are reaped.
ALTER TABLE incus_upload_sessions
    ADD COLUMN IF NOT EXISTS staged_parts JSONB;

COMMENT ON COLUMN incus_upload_sessions.staged_parts IS
    'Chunk objects on the repository storage backend, in commit order '
    '(#4302). NULL for a session started by an earlier version, whose chunks '
    'are in the local storage_temp_path file.';

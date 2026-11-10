-- #4883: sign-out revokes the session's access tokens, not only its refresh
-- tokens.
--
-- An access JWT minted as half of a token pair carries its refresh token's
-- `family_id` (#4831). Every access-token validation checks that family here
-- (one primary-key lookup per cache miss; `auth_service` caches the verdict
-- for SESSION_REVOCATION_CACHE_TTL_SECS, 5 s by default), so a signed-out
-- session's access token is refused on every replica within that bound
-- instead of staying valid until it expires.
--
-- Rows are written by the session-scoped revocations only: logout
-- (`revoke_refresh_token_family`) and refresh-token replay detection
-- (`refresh_tokens`). The user-wide revocation
-- (`revoke_all_refresh_token_families`: password change or reset,
-- deactivation, TOTP enable/disable) deliberately writes nothing: those
-- events already refuse access tokens through the credential watermark and
-- `users.is_active`, and the TOTP toggle exempts the calling session's own
-- access token (#1370), which a family row would kill.
--
-- A separate table rather than a column on `refresh_token_jti`: a sign-out
-- must be recorded even when the family has no jti row (pre-#1174 tokens,
-- pruned rows) or when every row was already revoked by a user-wide sweep.
--
-- `expires_at` is set past the last moment any access token of the family
-- can still be valid; the hourly refresh-token janitor deletes rows once
-- they are past it plus its grace window.
--
-- Additive only: 1.11.x code never reads or writes this table, so a rolling
-- upgrade is safe (a sign-out handled by a 1.11.x replica during the upgrade
-- revokes the refresh family only, as before).
--
-- Rollback: DROP TABLE IF EXISTS session_family_revocations;
CREATE TABLE IF NOT EXISTS session_family_revocations (
    family_id UUID PRIMARY KEY,
    reason TEXT NOT NULL
        CHECK (reason IN ('logout', 'refresh_replay')),
    revoked_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);

-- Janitor scans by expires_at.
CREATE INDEX IF NOT EXISTS session_family_revocations_expires_at_idx
    ON session_family_revocations (expires_at);

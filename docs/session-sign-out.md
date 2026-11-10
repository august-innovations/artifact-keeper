# Signing out and session access tokens

A login (password, LDAP, SSO, device authorization) returns a token pair: a
short-lived access token (`JWT_ACCESS_TOKEN_EXPIRY_MINUTES`, default 30) and a
refresh token (`JWT_REFRESH_TOKEN_EXPIRY_DAYS`, default 7). Both carry the
session's identifier (its refresh-token family).

## What ends a session

| Event | Refresh token | Access token |
|---|---|---|
| `POST /api/v1/auth/logout` | refused at once | refused (see below) |
| Refresh-token replay detected (a consumed refresh token presented again) | every token of the session refused | refused (see below) |
| Password change, admin password reset, forced password change | every session of the user refused | every session refused within 5 s on every replica |
| Admin deactivation or deletion | every session refused | refused on the next request |
| TOTP enable or disable | every session refused | every other session refused; the session that made the change keeps its access token until it expires |

Logout accepts the refresh token in the body or the `ak_refresh_token`
cookie, or no refresh token at all: the access token that authenticates the
request identifies the session. Logout clears the browser's session cookies,
and it ends only that session: the user's other logins keep working.

API tokens (`POST /api/v1/auth/tokens`) are not sessions and are not affected
by logout; revoke them explicitly.

## Cross-replica bound

After a logout or a replay detection, the session's access token is refused
at once on the replica that handled it. Every other replica caches a "still
signed in" verdict per session for `SESSION_REVOCATION_CACHE_TTL_SECS`
seconds and refuses the token at the latest when that verdict expires.

| Setting | Default | Range | Effect |
|---|---|---|---|
| `SESSION_REVOCATION_CACHE_TTL_SECS` | `5` | `0` to `300` (larger values are clamped) | How long a replica trusts its cached verdict. `0` disables the cache. |

The trade-off: each cache miss costs one primary-key lookup in
`session_family_revocations`, so at most one lookup per session per TTL per
replica. A smaller TTL shortens the window in which another replica still
accepts a signed-out access token; a larger one saves lookups.

Revocations are kept until every token of the session has expired (refresh
lifetime plus an hour), then pruned by the hourly refresh-token janitor.

## Rolling upgrades

Migration 294 adds the `session_family_revocations` table. Replicas still on
1.11.x neither write nor read it: a logout they handle revokes the refresh
token only, as before, until the upgrade completes.

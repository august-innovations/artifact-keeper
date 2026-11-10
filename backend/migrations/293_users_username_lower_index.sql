-- Case-insensitive username lookup for the LDAP Basic-auth passthrough (#3830).
--
-- Before binding, the passthrough refuses a submitted directory username that
-- belongs to a local/SSO account, so a mistyped local password cannot spend a
-- same-named directory account's failed-password budget. AD treats usernames
-- case-insensitively, so that check must too. The existing case-sensitive
-- `idx_users_username` cannot serve `lower(username) = lower($1)`.
CREATE INDEX IF NOT EXISTS idx_users_username_lower ON users (lower(username));

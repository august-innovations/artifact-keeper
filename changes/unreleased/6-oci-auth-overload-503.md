---
section: Fixed
issues: [#6]
---
- **`docker login` against an exhausted database pool now gets a retryable 503 instead of `invalid username or password`** (#6). `/v2/token`, `GET /v2/` with a bearer, and the bearer and JWT-as-password steps of `/v2` request authentication treated a pool timeout or bcrypt-capacity shed as a bad credential: they fell through to the next credential form (for an API token, the password check that bumps `failed_login_attempts`) and answered 401. An overloaded auth step now answers `503 UNAVAILABLE` at once; a genuinely wrong credential still gets the same 401.

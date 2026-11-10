# Detection signals

Signals added for the detection gaps the 1.11.0 red team found (#4677). The
audit actions are described in [audit-events.md](../audit-events.md).
The connection, rate-limit, SSRF, rejected-credential and proxy-scan series
(#4676), and the note on querying counters across restarts, are in
[detection-metrics.md](detection-metrics.md).

## Refused logins

A login refused 429 by the per-account login limiter or the global login
backstop, on `POST /api/v1/auth/login` or the OCI `/v2/token` exchange, writes
a `LOGIN_RATE_LIMITED` audit row and a `WARN` line on the `security` log
target. The row and the line are written at most once per source IP per
limiter window (once per window in total for the global backstop), so a
throttled brute-force burst is visible without the limiter turning into an
audit-table flood. Later refusals in the same window log at `DEBUG`.

## Permission condition evaluations

| Metric | Type | Labels | Moves when |
|---|---|---|---|
| `ak_permission_condition_evaluations_total` | counter | `condition`, `outcome` | A permission decision evaluated at least one rule carrying a condition. |

`condition` is `allowed_cidrs`, the only condition kind (#1849). `outcome`:

- `matched`: the request's verified client IP is inside at least one
  conditioned rule's ranges, so that rule applied.
- `not_matched`: the client IP is known and no conditioned rule covers it.
- `unknown_ip`: the client IP is not verified (a background job, or a request
  through a trusted proxy without `XFF_TRUSTED_FOR_AUTHZ`), so every
  conditioned rule failed closed.

One decision counts once, however many conditioned rules it saw. Decisions
over rules without conditions count nothing, so a deployment that uses no
conditions has no series. The cached permission path (`check_permission`)
reuses a result for 30 seconds per user, target and client IP; cache hits are
not evaluations and do not count. Each counted decision also logs a `DEBUG`
line (`permission condition evaluated`) with the resolver, principal, target,
client IP and rule tallies.

An operator can tell whether a conditioned grant is doing anything from the
ratio of `matched` to `not_matched`, and a steady `unknown_ip` rate behind a
reverse proxy usually means `XFF_TRUSTED_FOR_AUTHZ` is off.

## Joining gateway and backend logs

When the TCP peer is inside `RATE_LIMIT_TRUSTED_PROXY_CIDRS`, an inbound
`X-Request-ID` of 1 to 128 visible ASCII characters becomes the request's
correlation ID: it is echoed in `X-Correlation-ID`, recorded on the
`http_request` span and stamped on every audit row the request writes, so an
Envoy or NGINX access-log line joins the backend's logs on the same id.

The correlation ID is chosen in this order: an explicit `X-Correlation-ID`,
a trusted `X-Request-ID`, the trace id of `traceparent`, then a generated
UUID. When a caller sends its own `X-Correlation-ID` through the gateway, the
gateway's id is still recorded on the span as `request_id`. A peer outside the
list (or any peer while the list is empty) cannot set the correlation ID with
`X-Request-ID`.

## External findings ingest

A successful `POST /api/v1/security/findings/external` writes an
`EXTERNAL_FINDINGS_INGESTED` audit row naming the acting principal, the
artifact, the scanner, the severity and class counts and the `MAL-*` ids it
classed `malicious`, and logs an `INFO` line on the `security` target. Before
1.11.2 only refused submissions left a trace.

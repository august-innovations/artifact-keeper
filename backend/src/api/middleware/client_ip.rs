//! Request-scoped client-IP context (#3888), in two trust grades.
//!
//! [`client_ip_context_middleware`] resolves the request's client IP once
//! and scopes it as a task-local around the downstream request future, so
//! anything `.await`ed while handling the request (handlers, services,
//! audit emitters, permission gates) observes the same address without
//! threading it through every signature. This mirrors the correlation-ID
//! task-local in [`crate::api::middleware::tracing`].
//!
//! Two grades are scoped, because the consumers have different blast radii
//! when `X-Forwarded-For` cannot be verified:
//!
//! * **Best-effort** — [`current_client_ip`]. The
//!   [`resolve_client_ip_addr`] walk: the socket peer from `ConnectInfo`,
//!   with `X-Forwarded-For` believed only when the peer falls inside a
//!   configured trusted-proxy CIDR (the `RATE_LIMIT_TRUSTED_PROXY_CIDRS`
//!   policy, shared with the rate limiter; see #2023,
//!   GHSA-8jm4-4x6c-6787). Feeds availability/attribution consumers
//!   (rate-limit keying, the upload guard, download telemetry), where a
//!   spoofed key costs the attacker their own budget, not someone else's
//!   access.
//! * **Authorization-grade** — [`current_verified_client_ip`]. The
//!   [`resolve_verified_client_ip`] contract: an XFF-derived address is
//!   produced ONLY when the operator has asserted (via
//!   `XFF_TRUSTED_FOR_AUTHZ`) that every trusted proxy appends the
//!   downstream peer it observed; behind a trusted proxy without that
//!   assertion the grade resolves to `None` ("unknown"). Feeds permission
//!   `allowed_cidrs` authorization (#1849) and auth-audit IP attribution
//!   — consumers that fail closed on `None`.
//!
//! Outside a request scope (startup, background jobs, detached
//! `tokio::spawn` tasks, tests that do not establish one) both grades
//! resolve to `None` — callers must treat `None` as "unknown", never as a
//! sentinel address.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    middleware::Next,
    response::Response,
};

use super::rate_limit::{resolve_client_ip_addr, CidrRange};

/// Client-IP resolution policy handed to [`client_ip_context_middleware`].
///
/// `trusted_proxies` is the `RATE_LIMIT_TRUSTED_PROXY_CIDRS` set the
/// deployment shares with the rate limiter; `xff_trusted_for_authz` is the
/// operator's `XFF_TRUSTED_FOR_AUTHZ` assertion that every one of those
/// proxies appends `X-Forwarded-For` (see [`resolve_verified_client_ip`]).
#[derive(Debug, Clone)]
pub struct ClientIpConfig {
    pub trusted_proxies: Vec<CidrRange>,
    pub xff_trusted_for_authz: bool,
}

/// The two trust grades of resolved client address; see the module docs.
#[derive(Debug, Clone, Copy)]
struct ClientIpContext {
    /// Best-effort address ([`current_client_ip`]): rate-limit keying,
    /// upload guard, telemetry.
    best: Option<IpAddr>,
    /// Authorization-grade address ([`current_verified_client_ip`]):
    /// `allowed_cidrs` (#1849) and auth-audit attribution only.
    verified: Option<IpAddr>,
}

tokio::task_local! {
    /// The resolved client-IP context of the request currently being
    /// handled.
    ///
    /// [`client_ip_context_middleware`] scopes this around the downstream
    /// request future; the scope wraps the future, not the OS task, so it
    /// is correct under HTTP/2 multiplexing. A future detached with
    /// `tokio::spawn` does NOT inherit the value, which fails closed for
    /// IP-conditioned authorization (#1849): code that needs an address in
    /// a detached task must capture it first with [`current_client_ip`] /
    /// [`current_verified_client_ip`].
    static CURRENT_CLIENT_IP: ClientIpContext;
}

/// The best-effort client IP of the in-flight request, or `None` when
/// called outside a request scope (background jobs, startup, detached
/// tasks) or when the address could not be resolved. Mirrors
/// [`crate::api::middleware::tracing::current_correlation_id`].
///
/// This grade is for availability/attribution consumers (rate-limit
/// keying, upload guard, telemetry). Authorization and auth-audit IP
/// attribution MUST use [`current_verified_client_ip`] instead: behind a
/// non-appending proxy this address can be attacker-controlled.
pub fn current_client_ip() -> Option<IpAddr> {
    CURRENT_CLIENT_IP.try_with(|ctx| ctx.best).ok().flatten()
}

/// The authorization-grade client IP of the in-flight request
/// ([`resolve_verified_client_ip`]), or `None` outside a request scope or
/// when the address is not verified. ONLY permission `allowed_cidrs`
/// authorization (#1849) and auth-audit IP attribution may consume this
/// grade; `None` means "client unknown" and those consumers fail closed on
/// it, never treating it as a sentinel address.
pub fn current_verified_client_ip() -> Option<IpAddr> {
    CURRENT_CLIENT_IP
        .try_with(|ctx| ctx.verified)
        .ok()
        .flatten()
}

/// Runs `fut` with BOTH grades resolving to `ip` — an explicitly scoped
/// address asserts a verified fact (the caller established its caller's
/// address out of band), so it is authorization-grade by construction.
/// Signature and semantics are unchanged for existing callers; the
/// middleware uses [`with_client_ip_context`] to scope the two grades
/// independently.
pub async fn with_client_ip_scope<F: Future>(ip: Option<IpAddr>, fut: F) -> F::Output {
    with_client_ip_context(ip, ip, fut).await
}

/// Runs `fut` with [`current_client_ip`] resolving to `best` and
/// [`current_verified_client_ip`] resolving to `verified` — the scoping
/// [`client_ip_context_middleware`] applies to each request. Public so
/// tests (and any future non-HTTP entry point that can vouch for one grade
/// but not the other) can exercise the fail-closed paths — best-effort
/// address present, verified `None` — without standing up a router.
pub async fn with_client_ip_context<F: Future>(
    best: Option<IpAddr>,
    verified: Option<IpAddr>,
    fut: F,
) -> F::Output {
    CURRENT_CLIENT_IP
        .scope(ClientIpContext { best, verified }, fut)
        .await
}

/// Authorization-grade client-IP resolution: the ONLY resolution
/// authorization (#1849 `allowed_cidrs`) and auth-audit IP attribution may
/// consume.
///
/// Impossibility proof (do not "simplify" this back to the rate-limit
/// walk): behind a reverse proxy the `X-Forwarded-For` chain is verifiable
/// ONLY if every trusted hop appended the downstream peer it observed —
/// then the rightmost-untrusted walk lands on a token a trusted hop
/// vouched for. But nothing in the request distinguishes an appending
/// proxy from one that passes client-supplied XFF through untouched, nor
/// from a SNAT that destroyed the real peer address before the proxy saw
/// it: the bytes on the wire are identical, so no pure function of
/// (headers, peer, static config) can verify the chain. Trusting XFF for
/// authorization therefore requires an explicit operator assertion that
/// the topology appends — the `XFF_TRUSTED_FOR_AUTHZ` flag — and without
/// it this function fails closed (`None`, "client unknown") for every
/// request that arrived through a trusted proxy.
///
/// Truth table:
///
/// * `peer` present and NOT in `cfg.trusted_proxies` → `Some(peer)`: a
///   direct client; XFF ignored — the TCP peer is verified by the kernel.
/// * `peer` present and trusted, `xff_trusted_for_authz` OFF → `None`
///   regardless of XFF (the fail-closed core).
/// * `peer` present and trusted, flag ON → the
///   [`resolve_client_ip_addr`] walk (the rightmost-untrusted token),
///   falling back to `Some(peer)` when no usable token exists — preserves
///   the legitimate appending-proxy case.
/// * no `peer` → `None`: the no-`ConnectInfo` XFF fallback in
///   [`resolve_client_ip_addr`] is a test-harness convenience, NOT
///   authorization-grade.
pub(crate) fn resolve_verified_client_ip(
    headers: &axum::http::HeaderMap,
    peer: Option<IpAddr>,
    cfg: &ClientIpConfig,
) -> Option<IpAddr> {
    let peer = peer?;
    if !cfg.trusted_proxies.iter().any(|cidr| cidr.contains(peer)) {
        // Direct client: the kernel-verified TCP peer, XFF ignored.
        return Some(peer);
    }
    if !cfg.xff_trusted_for_authz {
        // Trusted proxy but no operator assertion that it appends: the
        // chain is unverifiable — fail closed.
        return None;
    }
    resolve_client_ip_addr(headers, Some(peer), &cfg.trusted_proxies).or(Some(peer))
}

/// Resolve the request's client IP (both grades) and scope them for the
/// downstream future.
///
/// Layered once, globally, next to `correlation_id_middleware` in
/// `create_router`, so every route (`/api/v1`, `/v2`, native formats) runs
/// inside the scope. The best-effort grade is the trusted-proxy-aware
/// [`resolve_client_ip_addr`]: the TCP peer is authoritative and a spoofed
/// `X-Forwarded-For` from an untrusted peer is ignored. The
/// authorization-grade grade is [`resolve_verified_client_ip`]: behind a
/// trusted proxy it yields `None` (fail closed) unless the operator has
/// asserted the topology appends XFF (`XFF_TRUSTED_FOR_AUTHZ`).
pub async fn client_ip_context_middleware(
    State(cfg): State<Arc<ClientIpConfig>>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    let best = resolve_client_ip_addr(request.headers(), peer, &cfg.trusted_proxies);
    let verified = resolve_verified_client_ip(request.headers(), peer, &cfg);
    with_client_ip_context(best, verified, next.run(request)).await
}
#[allow(clippy::disallowed_methods)]
// streaming-invariant: test module exempt — buffering response bodies in test assertions is not an artifact path (#1608)
#[cfg(ak_test_shard = "services-2")]
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::HeaderMap, routing::get, Router};
    use std::net::Ipv4Addr;
    use tower::ServiceExt;

    fn req(peer: Option<&str>, xff: Option<&str>) -> Request {
        let mut builder = Request::builder().uri("/");
        if let Some(xff) = xff {
            builder = builder.header("x-forwarded-for", xff);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            let addr: SocketAddr = peer.parse().unwrap();
            request.extensions_mut().insert(ConnectInfo(addr));
        }
        request
    }

    async fn ip_json() -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "ip": current_client_ip().map(|ip| ip.to_string()),
            "verified_ip": current_verified_client_ip().map(|ip| ip.to_string()),
        }))
    }

    /// Runs one request through the middleware with the given trusted-proxy
    /// CIDRs and `XFF_TRUSTED_FOR_AUTHZ` setting, returning both grades as
    /// the handler observed them.
    async fn observed(
        trusted: &[&str],
        xff_trusted_for_authz: bool,
        request: Request,
    ) -> serde_json::Value {
        let cfg = Arc::new(ClientIpConfig {
            trusted_proxies: trusted
                .iter()
                .map(|c| CidrRange::parse(c).unwrap())
                .collect(),
            xff_trusted_for_authz,
        });
        let app =
            Router::new()
                .route("/", get(ip_json))
                .layer(axum::middleware::from_fn_with_state(
                    cfg,
                    client_ip_context_middleware,
                ));
        let response = app.oneshot(request).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    }

    /// The best-effort grade ([`current_client_ip`]) the handler observed.
    async fn observed_ip(
        trusted: &[&str],
        xff_trusted_for_authz: bool,
        request: Request,
    ) -> Option<String> {
        observed(trusted, xff_trusted_for_authz, request)
            .await
            .get("ip")
            .unwrap()
            .as_str()
            .map(str::to_string)
    }

    /// The authorization-grade grade ([`current_verified_client_ip`]) the
    /// handler observed.
    async fn observed_verified_ip(
        trusted: &[&str],
        xff_trusted_for_authz: bool,
        request: Request,
    ) -> Option<String> {
        observed(trusted, xff_trusted_for_authz, request)
            .await
            .get("verified_ip")
            .unwrap()
            .as_str()
            .map(str::to_string)
    }

    #[tokio::test]
    async fn test_current_client_ip_is_none_outside_a_scope() {
        assert!(current_client_ip().is_none());
        assert!(current_verified_client_ip().is_none());
    }

    #[tokio::test]
    async fn test_with_client_ip_scope_bounds_the_value() {
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        let seen = with_client_ip_scope(Some(ip), async { current_client_ip() }).await;
        assert_eq!(seen, Some(ip));
        // The value must not leak past the scope.
        assert!(current_client_ip().is_none());
    }

    #[tokio::test]
    async fn test_middleware_scopes_the_tcp_peer() {
        let seen = observed_ip(&[], false, req(Some("198.51.100.20:443"), None)).await;
        assert_eq!(seen.as_deref(), Some("198.51.100.20"));
    }

    #[tokio::test]
    async fn test_middleware_ignores_xff_from_an_untrusted_peer() {
        // No trusted proxies configured: a spoofed XFF must not steer the
        // resolved address away from the real peer (#2023).
        let seen = observed_ip(
            &[],
            false,
            req(Some("198.51.100.20:443"), Some("192.0.2.33")),
        )
        .await;
        assert_eq!(seen.as_deref(), Some("198.51.100.20"));
    }

    #[tokio::test]
    async fn test_middleware_believes_xff_from_a_trusted_proxy() {
        let seen = observed_ip(
            &["198.51.100.0/24"],
            false,
            req(Some("198.51.100.20:443"), Some("192.0.2.33")),
        )
        .await;
        assert_eq!(seen.as_deref(), Some("192.0.2.33"));
    }

    #[tokio::test]
    async fn test_middleware_yields_none_when_nothing_resolves() {
        // No ConnectInfo (test topology) and no parseable XFF: unknown, never
        // a sentinel.
        let seen = observed_ip(&[], false, req(None, None)).await;
        assert_eq!(seen, None);
    }

    #[tokio::test]
    async fn test_middleware_falls_back_to_parseable_xff_without_peer() {
        // Direct-test / pre-ConnectInfo topology, matching resolve_client_ip_addr.
        let seen = observed_ip(&[], false, req(None, Some("192.0.2.33"))).await;
        assert_eq!(seen.as_deref(), Some("192.0.2.33"));
    }

    #[test]
    fn test_headerless_request_never_panics() {
        // The middleware reads headers and extensions only; a bare request
        // must resolve to None.
        let headers = HeaderMap::new();
        assert!(resolve_client_ip_addr(&headers, None, &[]).is_none());
    }

    // -----------------------------------------------------------------
    // Two-grade resolution — XFF-derived addresses are
    // authorization-grade ONLY under the operator's XFF_TRUSTED_FOR_AUTHZ
    // assertion; everything else fails closed to "unknown".
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn trusted_peer_flag_off_spoofed_xff_fails_closed() {
        // Trusted peer, XFF_TRUSTED_FOR_AUTHZ off: the best-effort grade
        // still resolves the (attacker-controlled) XFF token for rate-limit
        // keying, but the authorization grade must be None — the chain
        // cannot be verified, so authz/audit see "unknown" and fail closed.
        let request = || req(Some("198.51.100.20:443"), Some("192.0.2.33"));
        let best = observed_ip(&["198.51.100.0/24"], false, request()).await;
        let verified = observed_verified_ip(&["198.51.100.0/24"], false, request()).await;
        assert_eq!(best.as_deref(), Some("192.0.2.33"));
        assert_eq!(verified, None);
    }

    #[tokio::test]
    async fn trusted_peer_flag_on_appending_chain_resolves_the_client() {
        // The operator asserted the topology appends: with XFF
        // "<attacker prefix>, <real client>" from a trusted peer, the
        // verified grade is the rightmost-untrusted token — the address the
        // outermost trusted proxy observed for the downstream client.
        let request = || req(Some("198.51.100.20:443"), Some("192.0.2.33, 203.0.113.7"));
        let best = observed_ip(&["198.51.100.0/24"], true, request()).await;
        let verified = observed_verified_ip(&["198.51.100.0/24"], true, request()).await;
        assert_eq!(best.as_deref(), Some("203.0.113.7"));
        assert_eq!(verified.as_deref(), Some("203.0.113.7"));
    }

    #[tokio::test]
    async fn untrusted_peer_is_verified_regardless_of_flag_and_xff() {
        // A direct client needs no assertion: the kernel-verified TCP peer
        // IS the verified address, XFF ignored, under either flag setting.
        for flag in [false, true] {
            let request = || req(Some("192.0.2.10:443"), Some("203.0.113.7"));
            let best = observed_ip(&["198.51.100.0/24"], flag, request()).await;
            let verified = observed_verified_ip(&["198.51.100.0/24"], flag, request()).await;
            assert_eq!(best.as_deref(), Some("192.0.2.10"));
            assert_eq!(verified.as_deref(), Some("192.0.2.10"));
        }
    }

    #[tokio::test]
    async fn no_peer_verifies_nothing() {
        // The no-ConnectInfo XFF fallback is a test-harness convenience for
        // the best-effort grade only; it is NOT authorization-grade, under
        // either flag setting.
        for flag in [false, true] {
            let request = || req(None, Some("192.0.2.33"));
            let best = observed_ip(&[], flag, request()).await;
            let verified = observed_verified_ip(&[], flag, request()).await;
            assert_eq!(best.as_deref(), Some("192.0.2.33"));
            assert_eq!(verified, None);
        }
    }

    #[tokio::test]
    async fn trusted_peer_flag_on_without_xff_verifies_the_peer() {
        // Flag on but no XFF header: nothing to walk, so the verified grade
        // falls back to the kernel-verified TCP peer.
        let request = || req(Some("198.51.100.20:443"), None);
        let best = observed_ip(&["198.51.100.0/24"], true, request()).await;
        let verified = observed_verified_ip(&["198.51.100.0/24"], true, request()).await;
        assert_eq!(best.as_deref(), Some("198.51.100.20"));
        assert_eq!(verified.as_deref(), Some("198.51.100.20"));
    }

    #[tokio::test]
    async fn scope_helpers_populate_both_grades() {
        // with_client_ip_scope asserts a verified fact: both grades resolve
        // to the scoped address, and neither leaks past the scope.
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        let seen = with_client_ip_scope(Some(ip), async {
            (current_client_ip(), current_verified_client_ip())
        })
        .await;
        assert_eq!(seen, (Some(ip), Some(ip)));
        assert!(current_client_ip().is_none());
        assert!(current_verified_client_ip().is_none());

        // with_client_ip_context scopes the grades independently — the
        // fail-closed topology the middleware applies behind an unasserted
        // trusted proxy: best-effort present, verified unknown.
        let best = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 33));
        let seen = with_client_ip_context(Some(best), None, async {
            (current_client_ip(), current_verified_client_ip())
        })
        .await;
        assert_eq!(seen, (Some(best), None));
    }
}

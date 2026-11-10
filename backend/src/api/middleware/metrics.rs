//! Prometheus metrics middleware.
//!
//! Records per-request counters, histograms, and in-flight gauges using the
//! `metrics` crate. Path labels are taken from axum's `MatchedPath` when
//! available, which gives the route pattern (e.g. `/api/v1/repositories/:key`)
//! instead of the concrete URL. This avoids high-cardinality label explosion.
//! When no matched path is available (scanner traffic, 404s, fallback routes),
//! the label is collapsed to the fixed string `"unmatched"`.
//!
//! The OCI Distribution API is the exception (#4676): `/v2/*` is served by one
//! fallback handler that parses the path itself, so axum has no pattern for
//! it. Those requests are labelled with the distribution-spec template
//! (`/v2/{name}/manifests/{reference}`, `/v2/{name}/blobs/{digest}`, ...)
//! parsed by the same function the handler uses, so repository names, tags
//! and digests never become label values.

use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use metrics::{counter, gauge, histogram};

/// RAII guard that decrements the in-flight gauge on drop.
///
/// When a client disconnects mid-request, hyper cancels (drops) the response
/// future. Any code after the `.await` point is skipped, so a plain
/// `gauge!(...).decrement(1.0)` call would never execute. By putting the
/// decrement in a `Drop` impl we guarantee it runs regardless of how the
/// future completes — normal return *or* cancellation.
struct InFlightGuard {
    method: String,
    path: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        gauge!("ak_http_requests_in_flight",
               "method" => self.method.clone(),
               "path" => self.path.clone())
        .decrement(1.0);
    }
}

/// Axum middleware that records HTTP request metrics.
///
/// Emits the following metrics (all prefixed with `ak_`):
///
/// - `ak_http_requests_total` (counter): incremented when a request arrives.
///   Labels: `method`, `path`.
/// - `ak_http_responses_total` (counter): incremented after the response is
///   produced. Labels: `method`, `path`, `status`.
/// - `ak_http_request_duration_seconds` (histogram): request latency in
///   seconds. Labels: `method`, `path`, `status`. Exported with `_bucket`
///   series (bounds in `metrics_service::HTTP_REQUEST_DURATION_BUCKETS`), not
///   as a summary, so `histogram_quantile()` works across replicas (#3954).
/// - `ak_http_requests_in_flight` (gauge): number of requests currently being
///   processed. Labels: `method`, `path`.
///
/// The `path` label uses axum's `MatchedPath` (the route pattern) when
/// available. When no route matched (scanner probes, 404s, fallback routes),
/// it collapses to `"unmatched"` to prevent high-cardinality label explosion.
pub async fn metrics_middleware(request: Request, next: Next) -> Response {
    let method = request.method().to_string();

    // Prefer the matched route pattern for low-cardinality labels. When no
    // route matched, collapse to "unmatched" to avoid per-URL label explosion
    // from scanner traffic (which accounts for >99% of unmatched requests).
    let path = path_label(
        request
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str),
        request.uri().path(),
    );

    let start = Instant::now();

    counter!("ak_http_requests_total", "method" => method.clone(), "path" => path.clone())
        .increment(1);
    gauge!("ak_http_requests_in_flight", "method" => method.clone(), "path" => path.clone())
        .increment(1.0);

    // Guard ensures decrement happens even if the future is cancelled.
    let _guard = InFlightGuard {
        method: method.clone(),
        path: path.clone(),
    };

    let response = next.run(request).await;

    let duration = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    histogram!(
        crate::services::metrics_service::HTTP_REQUEST_DURATION_METRIC,
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status.clone(),
    )
    .record(duration);
    counter!(
        "ak_http_responses_total",
        "method" => method,
        "path" => path,
        "status" => status,
    )
    .increment(1);

    // _guard drops here, firing exactly one decrement in all cases:
    // normal completion and future cancellation.
    response
}

/// The `path` label for a request: its OCI route template when it is an OCI
/// Distribution request with no route pattern of its own, else axum's matched
/// pattern, else `"unmatched"`.
fn path_label(matched: Option<&str>, uri_path: &str) -> String {
    let routed_by_pattern = matched.is_some_and(|m| !m.contains('*'));
    if !routed_by_pattern {
        if let Some(template) = oci_route_template(uri_path) {
            return template.to_string();
        }
    }
    matched.unwrap_or("unmatched").to_string()
}

/// The distribution-spec route template for an OCI `/v2/...` request path,
/// or `None` when the path is not under `/v2/` or is not an OCI operation
/// the handler recognises (those stay `"unmatched"`).
fn oci_route_template(uri_path: &str) -> Option<&'static str> {
    let rest = uri_path.strip_prefix("/v2/")?;
    let (name, operation, reference) = crate::api::handlers::oci_v2::parse_oci_path(rest)?;
    if name.is_empty() {
        return None;
    }
    let has_reference = reference.is_some_and(|r| !r.is_empty());
    Some(match (operation.as_str(), has_reference) {
        ("manifests", _) => "/v2/{name}/manifests/{reference}",
        ("blobs", _) => "/v2/{name}/blobs/{digest}",
        ("uploads", true) => "/v2/{name}/blobs/uploads/{uuid}",
        ("uploads", false) => "/v2/{name}/blobs/uploads/",
        ("tags", _) => "/v2/{name}/tags/list",
        ("referrers", _) => "/v2/{name}/referrers/{digest}",
        _ => return None,
    })
}

#[cfg(ak_test_shard = "services-2")]
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, middleware, routing::get, Router};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use tower::ServiceExt;

    /// A captured metric: (name, labels, value).
    type CapturedMetric = (String, Vec<(String, String)>, DebugValue);

    async fn test_handler() -> &'static str {
        "OK"
    }

    /// Run the middleware against `app` with a `DebuggingRecorder` and return
    /// the captured counter values keyed by (name, labels).
    fn run_with_recorder(app: Router, uri: &str) -> Vec<CapturedMetric> {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
                    let _ = app.oneshot(request).await.unwrap();
                });
        });

        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(ck, _, _, value)| {
                let (_kind, key) = ck.into_parts();
                let name = key.name().to_string();
                let labels: Vec<(String, String)> = key
                    .into_parts()
                    .1
                    .into_iter()
                    .map(|l| (l.key().to_string(), l.value().to_string()))
                    .collect();
                (name, labels, value)
            })
            .collect()
    }

    #[test]
    fn test_middleware_returns_response() {
        // Install a no-op recorder so the metrics macros don't panic when no
        // global recorder is set. In production, init_metrics() sets one up.
        let _ = metrics::NoopRecorder;

        let app = Router::new()
            .route("/test", get(test_handler))
            .layer(middleware::from_fn(metrics_middleware));

        let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let response = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { app.oneshot(request).await.unwrap() });
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[test]
    fn test_middleware_handles_not_found() {
        let _ = metrics::NoopRecorder;

        let app = Router::new()
            .route("/exists", get(test_handler))
            .layer(middleware::from_fn(metrics_middleware));

        let request = Request::builder()
            .uri("/does-not-exist")
            .body(Body::empty())
            .unwrap();

        let response = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { app.oneshot(request).await.unwrap() });
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    /// Find a captured metric by name and return its labels.
    fn find_labels<'a>(metrics: &'a [CapturedMetric], name: &str) -> &'a [(String, String)] {
        metrics
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, labels, _)| labels.as_slice())
            .unwrap_or(&[])
    }

    #[test]
    fn test_matched_route_uses_route_pattern_label() {
        let app = Router::new()
            .route("/exists", get(test_handler))
            .layer(middleware::from_fn(metrics_middleware));

        let metrics = run_with_recorder(app, "/exists");

        let labels = find_labels(&metrics, "ak_http_requests_total");
        let path = labels
            .iter()
            .find(|(k, _)| k == "path")
            .map(|(_, v)| v.as_str())
            .expect("path label should exist");
        assert_eq!(path, "/exists");
    }

    #[test]
    fn test_unmatched_route_collapses_to_unmatched_label() {
        let app = Router::new()
            .route("/exists", get(test_handler))
            .layer(middleware::from_fn(metrics_middleware));

        // A scanner-style URL that doesn't match any route.
        let metrics = run_with_recorder(app, "/scanner-probe/.env");

        let labels = find_labels(&metrics, "ak_http_requests_total");
        let path = labels
            .iter()
            .find(|(k, _)| k == "path")
            .map(|(_, v)| v.as_str())
            .expect("path label should exist");
        assert_eq!(path, "unmatched");
    }

    fn path_label_of(metrics: &[CapturedMetric]) -> String {
        find_labels(metrics, "ak_http_requests_total")
            .iter()
            .find(|(k, _)| k == "path")
            .map(|(_, v)| v.clone())
            .expect("path label should exist")
    }

    /// The production shape of the OCI surface (#4676): `/v2` is a nested
    /// router whose concrete routes are `/token` and `/_catalog`, with every
    /// content operation served by its fallback, and the metrics layer on the
    /// outer router as in `main.rs`.
    fn oci_shaped_app() -> Router {
        let v2 = Router::new()
            .route("/token", get(test_handler))
            .route("/_catalog", get(test_handler))
            .fallback(test_handler);
        Router::new()
            .route("/v2/", get(test_handler))
            .nest("/v2", v2)
            .route("/api/v1/repositories/:key", get(test_handler))
            .layer(middleware::from_fn(metrics_middleware))
    }

    #[test]
    fn test_oci_requests_are_labelled_with_route_templates_4676() {
        let cases = [
            (
                "/v2/library/alpine/manifests/3.20",
                "/v2/{name}/manifests/{reference}",
            ),
            (
                "/v2/team/app/manifests/sha256:0123abcd",
                "/v2/{name}/manifests/{reference}",
            ),
            ("/v2/app/blobs/sha256:0123abcd", "/v2/{name}/blobs/{digest}"),
            ("/v2/a/b/c/blobs/uploads/", "/v2/{name}/blobs/uploads/"),
            (
                "/v2/app/blobs/uploads/5f0c6c1e-1b1a-4a8e-9f0e-2d1d2c3b4a59",
                "/v2/{name}/blobs/uploads/{uuid}",
            ),
            ("/v2/library/alpine/tags/list", "/v2/{name}/tags/list"),
            (
                "/v2/app/referrers/sha256:0123abcd",
                "/v2/{name}/referrers/{digest}",
            ),
        ];
        for (uri, template) in cases {
            let metrics = run_with_recorder(oci_shaped_app(), uri);
            assert_eq!(path_label_of(&metrics), template, "{uri}");
        }
    }

    #[test]
    fn test_oci_concrete_routes_and_other_paths_keep_their_labels_4676() {
        for (uri, label) in [
            ("/v2/token", "/v2/token"),
            ("/v2/_catalog", "/v2/_catalog"),
            ("/v2/", "/v2/"),
            (
                "/api/v1/repositories/maven-local",
                "/api/v1/repositories/:key",
            ),
            // Under /v2 but not an OCI operation: bounded, not the raw path.
            ("/v2/not/an/oci/operation", "unmatched"),
            ("/scanner-probe/.env", "unmatched"),
        ] {
            let metrics = run_with_recorder(oci_shaped_app(), uri);
            assert_eq!(path_label_of(&metrics), label, "{uri}");
        }
    }

    #[test]
    fn test_unmatched_routes_share_single_label_value() {
        // Two different unmatched URLs must produce the same "unmatched" path
        // label, proving cardinality is bounded.
        let app = Router::new()
            .route("/exists", get(test_handler))
            .layer(middleware::from_fn(metrics_middleware));

        let metrics1 = run_with_recorder(app.clone(), "/scanner/.git/config");
        let metrics2 = run_with_recorder(app, "/artifactory/libs-release/com/example");

        for metrics in [metrics1, metrics2] {
            let labels = find_labels(&metrics, "ak_http_requests_total");
            let path = labels
                .iter()
                .find(|(k, _)| k == "path")
                .map(|(_, v)| v.as_str())
                .expect("path label should exist");
            assert_eq!(path, "unmatched");
        }
    }
}

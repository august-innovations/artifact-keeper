//! OTLP log export for `tracing` events (#4337).
//!
//! [`super::init_with_otel`] installs this layer next to the stdout `fmt`
//! layer, so a deployment that sets `OTEL_EXPORTER_OTLP_ENDPOINT` ships its
//! diagnostics log to the collector as OpenTelemetry log records, over the
//! same OTLP endpoint (and protocol) as spans and metrics. Stdout output is
//! unchanged.
//!
//! Every exported record carries the trace context of the span the event was
//! emitted under (`trace_id` / `span_id`), which is what makes a log line
//! correlateable with its span in a backend such as Grafana Tempo — the
//! problem behind #4337. The context is read from the OTel span
//! `tracing-opentelemetry` stored in the *tracing* span
//! ([`tracing_opentelemetry::get_otel_context`], the same mechanism
//! [`db_spans`] uses), not from the ambient `opentelemetry::Context`, which
//! stays empty in a `tracing`-driven application.
//!
//! What reaches the export:
//!
//! * exactly the events the global `EnvFilter` lets through for stdout — the
//!   layer sits behind that filter, like every other layer;
//! * everything except `sqlx::query` events
//!   ([`db_spans::exported_to_otel`]): those carry SQL text, which stays
//!   stdout-only (#4455). Slow statements still appear as `CLIENT` spans via
//!   [`db_spans::DbQuerySpanLayer`];
//! * nothing the export itself produces: events of the exporter's transport
//!   stack ([`EXPORT_TRANSPORT_TARGETS`]) and events emitted while the SDK has
//!   suppressed telemetry are dropped, so shipping a batch never generates
//!   the next one.
//!
//! Only event fields are exported, not the fields of the enclosing spans.
//! Credentials are redacted where an event or span field is recorded
//! (`redact_url_for_diagnostics`, `redact_sensitive_params`), so a record
//! carries the same redacted values stdout prints.
//!
//! The layer is installed only on the OTLP path, and only when
//! `OTEL_LOGS_EXPORTER=otlp` (see [`super::SignalExporter`]).

use std::sync::OnceLock;

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, Severity};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::Context as OtelContext;
use opentelemetry_sdk::logs::{SdkLogRecord, SdkLogger};
use tracing::dispatcher::WeakDispatch;
use tracing::field::{Field, Visit};
use tracing::{Dispatch, Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Targets of the crates that carry an OTLP export (the gRPC and HTTP stacks
/// and the OpenTelemetry crates themselves). Their events never become log
/// records: exporting one would make the next export, a feedback loop the
/// `opentelemetry-appender-tracing` documentation warns about. Stdout still
/// prints them when `RUST_LOG` enables them.
const EXPORT_TRANSPORT_TARGETS: [&str; 10] = [
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry_otlp",
    "opentelemetry_http",
    "tonic",
    "h2",
    "hyper",
    "hyper_util",
    "tower",
    "reqwest",
];

/// Whether an event of `target` may be exported as a log record: not a
/// transport crate or one of its modules (`tower` matches `tower::buffer` but
/// not `tower_http`, whose request events are the backend's own).
fn exported_target(target: &str) -> bool {
    !EXPORT_TRANSPORT_TARGETS.iter().any(|krate| {
        target
            .strip_prefix(krate)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// Map a `tracing` level to the OTel severity that bears its name.
fn severity_of(level: &tracing::Level) -> Severity {
    match *level {
        tracing::Level::ERROR => Severity::Error,
        tracing::Level::WARN => Severity::Warn,
        tracing::Level::INFO => Severity::Info,
        tracing::Level::DEBUG => Severity::Debug,
        tracing::Level::TRACE => Severity::Trace,
    }
}

/// A `tracing` layer that forwards events to the OTLP logs pipeline.
pub(crate) struct OtelLogLayer {
    logger: SdkLogger,
    /// The dispatch this layer is part of, needed to look up the OTel context
    /// of a tracing span (`tracing_opentelemetry::get_otel_context`).
    dispatch: OnceLock<WeakDispatch>,
}

impl OtelLogLayer {
    pub(crate) fn new(logger: SdkLogger) -> Self {
        Self {
            logger,
            dispatch: OnceLock::new(),
        }
    }

    /// Attach the trace context of the span the event was emitted under, when
    /// there is one and it carries a valid OTel span. Events outside any span
    /// (background work, startup) are exported without one.
    fn record_trace_context<S>(
        &self,
        event: &Event<'_>,
        ctx: &Context<'_, S>,
        record: &mut SdkLogRecord,
    ) where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        let Some(dispatch) = self.dispatch.get().and_then(WeakDispatch::upgrade) else {
            return;
        };
        let Some(cx) = tracing_opentelemetry::get_otel_context(&span.id(), &dispatch) else {
            return;
        };
        let span = cx.span();
        let span_context = span.span_context();
        if !span_context.is_valid() {
            return;
        }
        record.set_trace_context(
            span_context.trace_id(),
            span_context.span_id(),
            Some(span_context.trace_flags()),
        );
    }
}

impl<S> Layer<S> for OtelLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_register_dispatch(&self, subscriber: &Dispatch) {
        let _ = self.dispatch.set(subscriber.downgrade());
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // SQL text never leaves stdout (#4455); the same filter that keeps it
        // off spans keeps it out of log records.
        if !super::db_spans::exported_to_otel(event.metadata()) {
            return;
        }
        let meta = event.metadata();
        if !exported_target(meta.target()) || OtelContext::is_current_telemetry_suppressed() {
            return;
        }
        let severity = severity_of(meta.level());
        let mut record = self.logger.create_log_record();
        record.set_target(meta.target());
        record.set_event_name(meta.name());
        record.set_severity_number(severity);
        record.set_severity_text(meta.level().as_str());
        self.record_trace_context(event, &ctx, &mut record);
        event.record(&mut EventVisitor {
            record: &mut record,
        });
        self.logger.emit(record);
    }
}

/// Copy the event's fields onto the log record: `message` becomes the body,
/// every other field becomes an attribute with its `tracing` type preserved
/// where the OTel value model has a counterpart (the same mapping
/// `opentelemetry-appender-tracing` uses; complex types fall back to their
/// `Debug` form).
struct EventVisitor<'a> {
    record: &'a mut SdkLogRecord,
}

impl EventVisitor<'_> {
    fn set_body_from_debug(&mut self, value: &dyn std::fmt::Debug) {
        self.record.set_body(format!("{value:?}").into());
    }
}

impl Visit for EventVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.set_body_from_debug(value);
        } else {
            self.record
                .add_attribute(field.name(), AnyValue::from(format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.record.set_body(AnyValue::from(value.to_owned()));
        } else {
            self.record
                .add_attribute(field.name(), AnyValue::from(value.to_owned()));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record.add_attribute(field.name(), value);
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record.add_attribute(field.name(), value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record.add_attribute(field.name(), value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        // OTel has no unsigned 64-bit integer attribute; a value beyond
        // i64 is stringified rather than wrapped.
        match i64::try_from(value) {
            Ok(signed) => self.record.add_attribute(field.name(), signed),
            Err(_) => self
                .record
                .add_attribute(field.name(), AnyValue::from(value.to_string())),
        }
    }

    fn record_error(&mut self, _field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record
            .add_attribute("exception.message", AnyValue::from(value.to_string()));
    }
}

#[cfg(ak_test_shard = "services-2")]
#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::logs::LoggerProvider as _;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::layer::SubscriberExt;

    /// Run `f` under a subscriber with the span export layer and the log
    /// layer, and return the log records exported meanwhile.
    fn exported_records(
        f: impl FnOnce(),
    ) -> Vec<opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource> {
        let exporter = InMemoryLogExporter::default();
        let logger_provider = SdkLoggerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer_provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(OtelLogLayer::new(logger_provider.logger("test")))
            .with(super::super::db_spans::otel_export_layer(
                tracer_provider.tracer("test"),
            ));
        tracing::subscriber::with_default(subscriber, f);
        logger_provider.force_flush().expect("flush");
        exporter.get_emitted_logs().expect("emitted logs")
    }

    fn attribute<'a>(
        record: &'a opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource,
        key: &str,
    ) -> Option<&'a AnyValue> {
        record
            .record
            .attributes_iter()
            .find(|(k, _)| k.as_str() == key)
            .map(|(_, v)| v)
    }

    #[test]
    fn an_event_becomes_a_record_with_body_severity_and_typed_fields() {
        let records = exported_records(|| {
            tracing::warn!(repository = "npm-remote", attempts = 3u64, "upstream slow");
        });
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(
            record.record.body(),
            Some(&AnyValue::from("upstream slow".to_owned()))
        );
        assert_eq!(record.record.severity_number(), Some(Severity::Warn));
        assert_eq!(
            attribute(record, "repository"),
            Some(&AnyValue::from("npm-remote".to_owned()))
        );
        assert_eq!(attribute(record, "attempts"), Some(&AnyValue::Int(3)));
        assert!(record.record.trace_context().is_none());
    }

    /// The point of #4337: a record emitted under a span carries that span's
    /// trace and span ids.
    #[test]
    fn an_event_inside_a_span_carries_its_trace_context() {
        let mut ids = None;
        let records = exported_records(|| {
            let span = tracing::info_span!("http_request");
            let _entered = span.enter();
            let cx = tracing_opentelemetry::OpenTelemetrySpanExt::context(&span);
            let span_context = cx.span().span_context().clone();
            ids = Some((span_context.trace_id(), span_context.span_id()));
            tracing::info!("request completed");
        });
        let (trace_id, span_id) = ids.expect("span ids");
        assert_eq!(records.len(), 1);
        let trace = records[0].record.trace_context().expect("trace context");
        assert_eq!(trace.trace_id, trace_id);
        assert_eq!(trace.span_id, span_id);
    }

    /// SQL text stays on stdout (#4455), and the transport's own events never
    /// become records (an export would otherwise produce the next one).
    #[test]
    fn sql_and_transport_events_are_not_exported() {
        let records = exported_records(|| {
            tracing::info!(target: "sqlx::query", summary = "SELECT 1", "query");
            tracing::info!(target: "h2::codec", "frame");
            tracing::info!(target: "hyper_util::client", "connecting");
            tracing::info!(target: "opentelemetry_sdk", "export failed");
            tracing::info!(target: "tower_http::trace", "kept");
        });
        let bodies: Vec<_> = records
            .iter()
            .filter_map(|r| r.record.body().cloned())
            .collect();
        assert_eq!(bodies, vec![AnyValue::from("kept".to_owned())]);
    }

    #[test]
    fn events_under_suppressed_telemetry_are_not_exported() {
        let records = exported_records(|| {
            let _suppressed = OtelContext::enter_telemetry_suppressed_scope();
            tracing::info!("from inside an export");
        });
        assert!(records.is_empty());
    }

    #[test]
    fn transport_targets_match_whole_crate_names() {
        assert!(!exported_target("tower"));
        assert!(!exported_target("tower::buffer"));
        assert!(exported_target("tower_http::trace"));
        assert!(!exported_target("opentelemetry_otlp::exporter"));
        assert!(exported_target(
            "artifact_keeper_backend::services::proxy_service"
        ));
    }
}

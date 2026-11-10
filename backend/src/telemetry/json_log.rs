//! The `LOG_FORMAT=json` diagnostics line (#2413 item 1, #4674).
//!
//! One JSON object per line. The layout `LOG_FORMAT=json` has emitted since
//! 1.10.0 is kept exactly: the stock `tracing-subscriber` JSON formatter still
//! writes `timestamp`, `level`, `fields` (the event's fields, nested),
//! `target`, `span` (the current span) and `spans` (the span stack), with the
//! same nesting and value types, so no existing parser breaks.
//!
//! [`FlatJson`] then appends a fixed set of request fields as TOP-LEVEL keys
//! ([`HOISTED`]), the keys a SIEM rule should target (#4674):
//!
//! * `method`, `uri`, `correlation_id`, `http.route` from the `http_request`
//!   root span, so they appear on every line logged while a request is
//!   handled;
//! * `status`, `user`, `ip`, `latency_ms` from the per-request
//!   `Request completed` event; `status` as an integer even though the nested
//!   `fields.status` keeps its historical string form.
//!
//! Each hoisted key takes the event's own field first, then the OUTERMOST span
//! carrying it, so an inner span's `uri` (an upstream URL, say) never masks the
//! request's. A key nothing in scope records is omitted. None of the hoisted
//! names collide with a stock key.
//!
//! The field reference lives in `docs/operations/logging.md`.

use std::fmt::{self, Write as _};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::{Format, Json, JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

/// The request fields appended as top-level keys, in output order.
pub(crate) const HOISTED: [&str; 9] = [
    "method",
    "uri",
    "status",
    "correlation_id",
    "http.route",
    "user",
    "ip",
    "latency_ms",
    "request_id",
];

/// The stock JSON event format plus the top-level [`HOISTED`] request fields.
/// Paired with [`JsonFields`] (what `fmt::layer().json()` installs), which
/// stores each span's recorded fields as a JSON object string.
#[derive(Debug)]
pub(crate) struct FlatJson {
    stock: Format<Json>,
}

impl Default for FlatJson {
    fn default() -> Self {
        Self {
            // The same defaults `fmt::layer().json()` used before #4674.
            stock: tracing_subscriber::fmt::format().json(),
        }
    }
}

impl<S> FormatEvent<S, JsonFields> for FlatJson
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, JsonFields>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut line = String::new();
        self.stock
            .format_event(ctx, Writer::new(&mut line), event)?;

        let hoisted = hoisted_fields(ctx, event);
        let body = line.trim_end();
        let Some(open) = body.strip_suffix('}').filter(|_| !hoisted.is_empty()) else {
            // Nothing to add (or an unexpected shape): the stock line as is.
            return writer.write_str(&line);
        };
        let mut out = String::with_capacity(line.len() + 128);
        out.push_str(open);
        let mut needs_comma = !open.trim_end().ends_with('{');
        for (key, value) in hoisted {
            if needs_comma {
                out.push(',');
            }
            needs_comma = true;
            // Serialising a &str or a `Value` cannot fail.
            out.push_str(&serde_json::to_string(key).map_err(|_| fmt::Error)?);
            out.push(':');
            out.push_str(&serde_json::to_string(&value).map_err(|_| fmt::Error)?);
        }
        out.push('}');
        writeln!(writer, "{out}")
    }
}

/// The [`HOISTED`] fields in scope for `event`: the event's own value first,
/// else the outermost span that recorded one.
fn hoisted_fields<S>(
    ctx: &FmtContext<'_, S, JsonFields>,
    event: &Event<'_>,
) -> Vec<(&'static str, Value)>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let mut found = EventFields::default();
    event.record(&mut found);
    let mut values = found.0;

    if let Some(scope) = ctx.event_scope() {
        for span in scope.from_root() {
            let ext = span.extensions();
            let Some(fields) = ext.get::<FormattedFields<JsonFields>>() else {
                continue;
            };
            if fields.is_empty() {
                continue;
            }
            let Ok(map) = serde_json::from_str::<Map<String, Value>>(fields) else {
                continue;
            };
            for key in HOISTED {
                if let Some(value) = map.get(key) {
                    values
                        .entry(key.to_string())
                        .or_insert_with(|| value.clone());
                }
            }
        }
    }

    HOISTED
        .iter()
        .filter_map(|&key| {
            let value = values.remove(key)?;
            Some((
                key,
                if key == "status" {
                    as_integer(value)
                } else {
                    value
                },
            ))
        })
        .collect()
}

/// `status` is recorded with `%` (a string) on the `Request completed` line;
/// the top-level copy is a number so a rule can compare it (`status >= 500`).
fn as_integer(value: Value) -> Value {
    match &value {
        Value::String(s) => s.parse::<u64>().map(Value::from).unwrap_or(value),
        _ => value,
    }
}

/// Collects an event's [`HOISTED`] fields as JSON values.
#[derive(Default)]
struct EventFields(Map<String, Value>);

impl EventFields {
    fn insert(&mut self, field: &Field, value: Value) {
        if HOISTED.contains(&field.name()) {
            self.0.insert(field.name().to_string(), value);
        }
    }
}

impl Visit for EventFields {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, Value::from(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, Value::from(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if HOISTED.contains(&field.name()) {
            let mut s = String::new();
            let _ = write!(s, "{value:?}");
            self.insert(field, Value::from(s));
        }
    }
}

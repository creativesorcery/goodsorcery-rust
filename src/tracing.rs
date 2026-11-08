//! A [`tracing_subscriber::Layer`] that turns tracing events into
//! breadcrumbs and, with performance monitoring on, database and HTTP spans
//! into request timings.
//!
//! ```no_run
//! use tracing_subscriber::prelude::*;
//! tracing_subscriber::registry()
//!     .with(goodsorcery::tracing::layer())
//!     .init();
//! ```
//!
//! What it recognises for performance monitoring, inside a request or job:
//!
//! - **sqlx** query events (target `sqlx::query`, with `db.statement` or
//!   `summary` and `elapsed`/`elapsed_secs`). sqlx logs statements at
//!   `DEBUG` by default: if a global filter (such as an `EnvFilter` added
//!   as its own layer) drops `DEBUG` events, this layer never sees them, so
//!   filter your other layers individually (`fmt::layer().with_filter(…)`).
//! - Spans with OpenTelemetry field names: `db.statement` or
//!   `db.query.text` (with `db.system` for the dialect) are queries;
//!   `http.request.method` (or `http.method`) with `url.full` (or
//!   `http.url`) are outgoing HTTP calls, with `http.response.status_code`
//!   (or `http.status_code`). Their duration is the span's open-to-close
//!   time.
//!
//! Breadcrumbs: events at `INFO` and above (configurable) become `log`
//! breadcrumbs with their message and fields (filtered like context);
//! query events become `sql` breadcrumbs with normalised SQL.
//!
//! Work done on another thread on behalf of a request or job is linked
//! back through its spans: a span created while a request or job runs
//! remembers it, and an event on a thread with no request bound goes to
//! the request of the span it was emitted in. sqlx's SQLite driver works
//! this way (each connection runs its queries on its own thread, inside
//! the caller's span). The axum layer and [`job`](crate::job) open a
//! `TRACE` span (target `goodsorcery`) for each request and job run so
//! there always is one: a global filter that drops it (or a global
//! `EnvFilter` without `goodsorcery=trace`) leaves SQLite queries untimed
//! unless the app opens its own spans inside the request.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::tracing::field::{Field, Visit};
use ::tracing::{Event, Level as TracingLevel, Subscriber, span};
use serde_json::Value;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::client;
use crate::event::Level;
use crate::scope::{self, Breadcrumb, Hub};
use crate::util::truncate;

/// The request or job run a span was created in (stored on the first span
/// of a chain created while it runs; children find it on their parents).
struct HubLink(Hub);

/// A span carrying `hub`, for the axum layer and [`job`](crate::job) to
/// enter while the unit of work runs. See the module docs.
pub(crate) fn unit_of_work_span(hub: &Hub) -> ::tracing::Span {
    scope::with_hub(hub.clone(), || ::tracing::trace_span!(target: "goodsorcery", "goodsorcery"))
}

/// The hub of the nearest span in `spans` (innermost first) that has one.
fn linked_hub<'a, R>(spans: impl IntoIterator<Item = tracing_subscriber::registry::SpanRef<'a, R>>) -> Option<Hub>
where
    R: LookupSpan<'a> + 'a,
{
    spans.into_iter().find_map(|span| span.extensions().get::<HubLink>().map(|link| link.0.clone()))
}

/// The hub bound to this thread, if it has a scope of its own.
fn bound_hub() -> Option<Hub> {
    scope::current_hub().filter(|hub| hub.scope.is_some())
}

fn same_unit(a: &Hub, b: &Hub) -> bool {
    match (&a.scope, &b.scope) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// The layer, with breadcrumbs from `INFO` up and performance spans on.
pub fn layer() -> GoodSorceryLayer {
    GoodSorceryLayer { breadcrumbs: true, breadcrumb_level: TracingLevel::INFO, performance: true }
}

/// See [`layer`].
#[derive(Debug, Clone)]
pub struct GoodSorceryLayer {
    breadcrumbs: bool,
    breadcrumb_level: TracingLevel,
    performance: bool,
}

impl GoodSorceryLayer {
    /// Turns breadcrumbs from tracing events on or off (default on).
    pub fn breadcrumbs(mut self, on: bool) -> Self {
        self.breadcrumbs = on;
        self
    }

    /// The least severe level that becomes a breadcrumb (default `INFO`).
    pub fn breadcrumb_level(mut self, level: TracingLevel) -> Self {
        self.breadcrumb_level = level;
        self
    }

    /// Turns span timing for performance monitoring on or off (default on;
    /// it only records while monitoring is on).
    pub fn performance(mut self, on: bool) -> Self {
        self.performance = on;
        self
    }
}

/// The fields this layer understands.
#[derive(Default)]
struct Fields {
    message: Option<String>,
    statement: Option<String>,
    summary: Option<String>,
    db_system: Option<String>,
    elapsed: Option<Duration>,
    rows: Option<u64>,
    http_method: Option<String>,
    url: Option<String>,
    status: Option<u16>,
    others: Vec<(String, Value)>,
}

impl Fields {
    fn set(&mut self, field: &Field, value: Value) {
        let text = || match &value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        match field.name() {
            "message" => self.message = Some(text()),
            "db.statement" | "db.query.text" => self.statement = Some(text()),
            "summary" => self.summary = Some(text()),
            "db.system" | "db.system.name" => self.db_system = Some(text()),
            "elapsed_secs" => self.elapsed = value.as_f64().and_then(|secs| Duration::try_from_secs_f64(secs).ok()),
            "elapsed" if self.elapsed.is_none() => self.elapsed = parse_duration(&text()),
            "duration_ms" if self.elapsed.is_none() => {
                self.elapsed = value.as_f64().and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
            }
            "rows_returned" | "db.response.returned_rows" => self.rows = value.as_u64(),
            "http.request.method" | "http.method" => self.http_method = Some(text()),
            "url.full" | "http.url" => self.url = Some(text()),
            "http.response.status_code" | "http.status_code" => {
                self.status = value.as_u64().and_then(|code| u16::try_from(code).ok()).or_else(|| text().parse().ok())
            }
            name if name.starts_with("log.") => {}
            name => {
                if self.others.len() < 20 {
                    self.others.push((name.to_string(), value));
                }
            }
        }
    }

    fn sql_text(&self) -> Option<(&str, bool)> {
        match (&self.statement, &self.summary) {
            (Some(statement), _) if !statement.trim().is_empty() => Some((statement.as_str(), false)),
            (_, Some(summary)) if !summary.trim().is_empty() => Some((summary.as_str(), summary.ends_with('…'))),
            _ => None,
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.set(field, Value::String(truncate(&format!("{value:?}"), 4_096)));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field, Value::String(truncate(value, 4_096)));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.set(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.set(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.set(field, serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.set(field, Value::Bool(value));
    }
}

/// `Duration`'s `Debug` form (`1.234567ms`, `12µs`, `2.5s`).
fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().ok()?;
    let secs = match unit {
        "ns" => value / 1e9,
        "µs" | "us" => value / 1e6,
        "ms" => value / 1e3,
        "s" => value,
        _ => return None,
    };
    Duration::try_from_secs_f64(secs).ok()
}

fn breadcrumb_level(level: &TracingLevel) -> Level {
    match *level {
        TracingLevel::ERROR => Level::Error,
        TracingLevel::WARN => Level::Warning,
        TracingLevel::INFO => Level::Info,
        _ => Level::Debug,
    }
}

/// A database or HTTP span being timed.
#[cfg(feature = "performance")]
struct Timing {
    recorder: Arc<crate::performance::recorder::Recorder>,
    start: Instant,
    fields: Fields,
    file: Option<&'static str>,
    line: Option<u32>,
}

impl<S> Layer<S> for GoodSorceryLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if !client::enabled_fast() || client::on_sdk_thread() {
            return;
        }
        let metadata = event.metadata();
        if metadata.target().starts_with("goodsorcery") {
            return;
        }
        let wants_breadcrumb = self.breadcrumbs && *metadata.level() <= self.breadcrumb_level;
        let is_query = metadata.target().starts_with("sqlx::query");
        if !(wants_breadcrumb || self.performance && is_query) {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        // On a thread working for a request without its scope bound (the
        // SQLite driver's connection thread), the event goes to the request
        // of the span it was emitted in.
        let linked = match bound_hub() {
            Some(_) => None,
            None => ctx.event_scope(event).and_then(|spans| linked_hub(spans)),
        };
        match linked {
            Some(hub) => scope::with_hub(hub, || self.handle_event(fields, metadata, wants_breadcrumb)),
            None => self.handle_event(fields, metadata, wants_breadcrumb),
        }
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if !client::enabled_fast() || client::on_sdk_thread() {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let hub = match bound_hub() {
            Some(hub) => {
                let parent_has_it = span.parent().is_some_and(|parent| {
                    parent.extensions().get::<HubLink>().is_some_and(|link| same_unit(&link.0, &hub))
                });
                if !parent_has_it {
                    span.extensions_mut().insert(HubLink(hub.clone()));
                }
                Some(hub)
            }
            None => linked_hub(span.scope()),
        };
        #[cfg(feature = "performance")]
        if self.performance {
            start_timing(attrs, &span, hub);
        }
    }

    #[cfg(feature = "performance")]
    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(timing) = span.extensions_mut().get_mut::<Timing>() {
                values.record(&mut timing.fields);
            }
        }
    }

    #[cfg(feature = "performance")]
    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(timing) = span.extensions_mut().remove::<Timing>() else { return };
        finish_timing(timing);
    }
}

impl GoodSorceryLayer {
    fn handle_event(&self, fields: Fields, metadata: &::tracing::Metadata<'_>, wants_breadcrumb: bool) {
        #[cfg(feature = "performance")]
        if self.performance {
            record_query_event(&fields);
        }
        if wants_breadcrumb {
            let breadcrumb = if fields.sql_text().is_some() {
                sql_breadcrumb(&fields)
            } else {
                log_breadcrumb(fields, metadata.name())
            };
            scope::configure_scope(|scope| scope.add_breadcrumb(breadcrumb.level(breadcrumb_level(metadata.level()))));
        }
    }
}

/// Starts timing a database or HTTP span created in a request or job (on
/// this thread, or linked through its parent spans).
#[cfg(feature = "performance")]
fn start_timing<'a, R: LookupSpan<'a> + 'a>(
    attrs: &span::Attributes<'_>,
    span: &tracing_subscriber::registry::SpanRef<'a, R>,
    hub: Option<Hub>,
) {
    let declared = attrs.metadata().fields();
    let interesting =
        ["db.statement", "db.query.text", "url.full", "http.url"].iter().any(|name| declared.field(name).is_some());
    if !interesting {
        return;
    }
    let recorder = crate::performance::current_recorder().or_else(|| {
        let recorder = hub?.recorder?;
        recorder.shared.on.load(std::sync::atomic::Ordering::Relaxed).then_some(recorder)
    });
    let Some(recorder) = recorder else { return };
    let mut fields = Fields::default();
    attrs.record(&mut fields);
    span.extensions_mut().insert(Timing {
        recorder,
        start: Instant::now(),
        fields,
        file: attrs.metadata().file(),
        line: attrs.metadata().line(),
    });
}

/// Records a timed span that just closed.
#[cfg(feature = "performance")]
fn finish_timing(timing: Timing) {
    let duration = timing.start.elapsed();
    let fields = &timing.fields;
    if let Some((sql, _)) = fields.sql_text() {
        let dialect = fields.db_system.as_deref().and_then(crate::performance::Dialect::from_name);
        // Where the span was declared is the call site only when that is
        // app code; a span declared inside a library (a database crate's
        // own instrumentation) resolves the first app frame instead.
        let site = match (timing.file, timing.line) {
            (Some(file), Some(line)) if declared_in_app(file) => {
                crate::performance::recorder::SiteSource::Static { file, line }
            }
            _ => crate::performance::recorder::SiteSource::Capture,
        };
        timing.recorder.record_sql(sql, timing.start, duration, dialect, fields.rows, None, site);
    } else if let Some(url) = fields.url.as_deref().filter(|url| url.contains("://")) {
        let method = fields.http_method.as_deref().unwrap_or("");
        timing.recorder.record_http(method, url, fields.status, timing.start, duration);
    }
}

/// Whether a span declared in `file` is the app's own code.
#[cfg(feature = "performance")]
fn declared_in_app(file: &str) -> bool {
    client::current().and_then(|client| client.paths.clone()).is_some_and(|paths| paths.declared_in_app(file))
}

/// A finished query event (sqlx): records it in the current request or job.
#[cfg(feature = "performance")]
fn record_query_event(fields: &Fields) {
    let (Some((sql, cut)), Some(elapsed)) = (fields.sql_text(), fields.elapsed) else { return };
    let Some(recorder) = crate::performance::current_recorder() else { return };
    let dialect = fields.db_system.as_deref().and_then(crate::performance::Dialect::from_name);
    let text = if cut { sql.trim_end_matches('…') } else { sql };
    recorder.record_sql(
        text,
        crate::performance::began(elapsed),
        elapsed,
        dialect,
        fields.rows,
        None,
        crate::performance::recorder::SiteSource::Capture,
    );
}

fn sql_breadcrumb(fields: &Fields) -> Breadcrumb {
    let message = match fields.elapsed {
        Some(elapsed) => format!("SQL query ({:.1}ms)", elapsed.as_secs_f64() * 1000.0),
        None => "SQL query".to_string(),
    };
    #[allow(unused_mut)]
    let mut breadcrumb = Breadcrumb::new("sql", message);
    #[cfg(feature = "performance")]
    if let Some((sql, cut)) = fields.sql_text() {
        let dialect = fields
            .db_system
            .as_deref()
            .and_then(crate::performance::Dialect::from_name)
            .or_else(crate::performance::default_dialect);
        let normalized = crate::performance::sql::normalize_cut(sql, dialect, cut);
        breadcrumb = breadcrumb.data("sql", truncate(&normalized, 1_000));
    }
    breadcrumb
}

fn log_breadcrumb(fields: Fields, name: &str) -> Breadcrumb {
    let message = fields.message.unwrap_or_else(|| name.to_string());
    let mut breadcrumb = Breadcrumb::new("log", message);
    for (key, value) in fields.others {
        breadcrumb = breadcrumb.data(key, value);
    }
    breadcrumb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_debug_durations() {
        assert_eq!(parse_duration("1.5ms"), Some(Duration::from_micros(1_500)));
        assert_eq!(parse_duration("12µs"), Some(Duration::from_micros(12)));
        assert_eq!(parse_duration("2s"), Some(Duration::from_secs(2)));
        assert_eq!(parse_duration("soon"), None);
    }
}

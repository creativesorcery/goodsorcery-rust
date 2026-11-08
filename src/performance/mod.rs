//! Performance monitoring (APM): per-minute metrics for every request and
//! job, and traces of the slowest ones.
//!
//! Turn it on with [`Config::performance_monitoring`](crate::Config::performance_monitoring)
//! (it is off by default). Requests are recorded by the
//! [axum layer](crate::axum::layer), jobs by [`job`](crate::job). Inside
//! them, database and HTTP time come from:
//!
//! - the [`tracing` layer](crate::tracing::layer) (feature `tracing`): sqlx
//!   query events, and spans with OpenTelemetry field names (`db.statement`
//!   or `db.query.text`; `http.request.method` with `url.full`);
//! - or this module's functions, from your own code: [`record_span`],
//!   [`record_sql`], [`record_http`] and [`start_span`].
//!
//! The app-code layer is the remainder: total time minus database, HTTP and
//! rendering, floored at 0.
//!
//! Privacy: metrics carry transaction names, route patterns and numbers
//! only. Traces carry normalised SQL (no literal values), HTTP method, host
//! and path (no query string or credentials), the request's method, path
//! and request id, and never params, headers, cookies or bodies.

pub(crate) mod aggregator;
pub(crate) mod flusher;
pub(crate) mod histogram;
pub(crate) mod recorder;
pub(crate) mod sql;
pub(crate) mod traces;

use std::collections::HashSet;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use aggregator::{Aggregator, Measurement};
use flusher::{FlushState, Flusher, MAX_BUILDS};
use recorder::{Kind, RecData, Recorder, SiteSource};
use traces::{Candidate, TraceBuffer, TraceContext};

pub use sql::{Dialect, normalize as normalize_sql};

use crate::backtrace::{Paths, SourceCache};
use crate::config::Config;
use crate::event::EventId;
use crate::runtime::RuntimeInfo;
use crate::transport::Transport;
use crate::util::{deadline_after, lock, log, millis, minute_of, truncate};

/// State shared by recorders, the client and the flusher thread.
pub(crate) struct Shared {
    pub(crate) on: AtomicBool,
    traces_on: bool,
    ignore: HashSet<String>,
    pub(crate) aggregator: Mutex<Aggregator>,
    pub(crate) traces: Mutex<TraceBuffer>,
    pub(crate) flush: Mutex<FlushState>,
    pub(crate) flush_signal: Condvar,
    pub(crate) flush_done: Condvar,
}

/// The running performance monitor, owned by the client.
pub(crate) struct Apm {
    pub(crate) shared: Arc<Shared>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Apm {
    pub(crate) fn start(
        config: &Config,
        transport: Arc<Transport>,
        paths: Arc<Paths>,
        runtime: RuntimeInfo,
    ) -> Option<Apm> {
        if !config.performance_monitoring {
            return None;
        }
        let shared = Arc::new(Shared {
            on: AtomicBool::new(true),
            traces_on: config.performance_traces,
            ignore: config.performance_ignore_transactions.iter().cloned().collect(),
            aggregator: Mutex::new(Aggregator::default()),
            traces: Mutex::new(TraceBuffer::default()),
            flush: Mutex::new(FlushState::default()),
            flush_signal: Condvar::new(),
            flush_done: Condvar::new(),
        });
        let context = TraceContext { paths, source: config.send_code_context.then(SourceCache::new), runtime };
        let flusher = Flusher::new(shared.clone(), transport, context);
        let handle = std::thread::Builder::new()
            .name("goodsorcery-apm".into())
            .spawn(move || {
                crate::client::mark_sdk_thread();
                flusher.run();
            })
            .map_err(|_| log::warn("could not start the performance thread; performance monitoring is off"))
            .ok()?;
        log::debug("performance monitoring is on");
        Some(Apm { shared, handle: Mutex::new(Some(handle)) })
    }

    /// Sends every minute recorded so far (the open one included), waiting
    /// up to `timeout`.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let deadline = deadline_after(timeout);
        let mut state = lock(&self.shared.flush);
        if state.stopped {
            return true;
        }
        state.requested += 1;
        let generation = state.requested;
        self.shared.flush_signal.notify_all();
        while state.flushed < generation && !state.stopped {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .shared
                .flush_done
                .wait_timeout(state, deadline - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        true
    }

    /// Sends everything and stops the thread, waiting up to `timeout`.
    pub(crate) fn close(&self, timeout: Duration) {
        let deadline = deadline_after(timeout);
        {
            let mut state = lock(&self.shared.flush);
            state.closing = true;
            self.shared.flush_signal.notify_all();
            while !state.stopped {
                let now = Instant::now();
                if now >= deadline {
                    log::warn("performance data was not sent before the shutdown timeout");
                    return;
                }
                state = self
                    .shared
                    .flush_done
                    .wait_timeout(state, deadline - now)
                    .map(|(guard, _)| guard)
                    .unwrap_or_else(|e| e.into_inner().0);
            }
        }
        self.shared.on.store(false, Ordering::Relaxed);
        if let Some(handle) = lock(&self.handle).take() {
            let _ = handle.join();
        }
    }
}

fn shared() -> Option<Arc<Shared>> {
    let client = crate::client::current_enabled()?;
    let shared = client.apm.as_ref()?.shared.clone();
    shared.on.load(Ordering::Relaxed).then_some(shared)
}

/// The recorder bound to this thread, while monitoring is on.
pub(crate) fn current_recorder() -> Option<Arc<Recorder>> {
    let recorder = crate::scope::current_hub()?.recorder?;
    recorder.shared.on.load(Ordering::Relaxed).then_some(recorder)
}

/// Starts recording a web request (`None` when monitoring is off or the
/// transaction is ignored).
pub(crate) fn start_web(
    name: &str,
    method: &str,
    route: &str,
    path: &str,
    request_id: Option<String>,
) -> Option<Arc<Recorder>> {
    let shared = shared()?;
    if shared.ignore.contains(name) {
        return None;
    }
    let mut data = RecData::new(Kind::Web, truncate(name, 255));
    data.method = Some(truncate(method, 16));
    data.route = Some(truncate(route, 255));
    data.path = Some(truncate(path, 1_024));
    data.request_id = request_id;
    Some(Arc::new(Recorder::new(shared, data)))
}

/// Starts recording a job run.
pub(crate) fn start_job(
    name: &str,
    adapter: Option<&str>,
    queue: Option<&str>,
    queue_wait: Option<Duration>,
    executions: Option<u32>,
) -> Option<Arc<Recorder>> {
    let shared = shared()?;
    if shared.ignore.contains(name) {
        return None;
    }
    let mut data = RecData::new(Kind::Job, truncate(name, 255));
    data.adapter = adapter.map(str::to_string);
    data.queue = queue.map(str::to_string);
    data.queue_wait = queue_wait;
    data.executions = executions;
    Some(Arc::new(Recorder::new(shared, data)))
}

/// Finishes a unit of work: adds it to the minute's metrics and, when it is
/// the slowest so far, hands it to the flusher to build its trace.
pub(crate) fn finish(recorder: &Arc<Recorder>, status: Option<u16>, failed: bool) {
    let duration = recorder.started.elapsed();
    let data = {
        let mut data = lock(&recorder.data);
        if data.finished {
            return;
        }
        let mut finished = RecData::new(data.kind, String::new());
        finished.finished = true;
        std::mem::replace(&mut *data, finished)
    };
    let mut data = data;
    data.status = status.or(data.status);
    data.failed = data.failed || failed;
    let shared = &recorder.shared;
    if !shared.on.load(Ordering::Relaxed) {
        return;
    }
    let app = duration.saturating_sub(data.db + data.http + data.view);
    let minute = minute_of(SystemTime::now());
    let measurement = Measurement {
        kind: data.kind,
        name: data.name.clone(),
        method: data.method.clone(),
        route: data.route.clone(),
        adapter: data.adapter.clone(),
        queue: data.queue.clone(),
        minute,
        duration_ms: millis(duration),
        db_ms: millis(data.db),
        http_ms: millis(data.http),
        view_ms: millis(data.view),
        app_ms: millis(app),
        failed: data.failed,
        queue_wait_ms: data.queue_wait.map(millis),
    };
    lock(&shared.aggregator).add(&measurement);
    if !shared.traces_on {
        return;
    }
    let Some(seq) = lock(&shared.traces).reserve(minute, data.kind, &data.name, duration) else { return };
    let candidate = Candidate { seq, minute, started_at: recorder.started_at, duration, app, data };
    let mut state = lock(&shared.flush);
    if state.builds.len() < MAX_BUILDS && !state.stopped {
        state.builds.push_back(candidate);
        shared.flush_signal.notify_all();
    }
}

/// Links the current unit of work to an error event (the first wins).
pub(crate) fn note_error(id: EventId) {
    if let Some(recorder) = current_recorder() {
        let mut data = lock(&recorder.data);
        if data.error_event_id.is_none() {
            data.error_event_id = Some(id);
        }
    }
}

/// What a span measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpanKind {
    /// A database query; the description is the SQL (normalised before it
    /// is sent).
    Sql,
    /// An outgoing HTTP call; the description is `"METHOD URL"` (only the
    /// method, host and path are kept).
    Http,
    /// Rendering a template or view; the description is its name.
    View,
    /// Enqueueing a job; the description is the job's name.
    Enqueue,
}

/// Records a span that just ended and took `duration`, in the current
/// request or job. Does nothing outside one, or with monitoring off.
///
/// ```
/// use std::time::Duration;
/// use goodsorcery::performance::{SpanKind, record_span};
/// record_span(SpanKind::Sql, Duration::from_millis(3), "SELECT * FROM users WHERE id = $1");
/// record_span(SpanKind::Http, Duration::from_millis(212), "GET https://api.example.com/v2/rates?zip=94107");
/// ```
#[track_caller]
pub fn record_span(kind: SpanKind, duration: Duration, description: &str) {
    let location = Location::caller();
    let Some(recorder) = current_recorder() else { return };
    record_into(&recorder, kind, description, None, began(duration), duration, location);
}

/// Records a database query that just ended. Repeated queries are grouped
/// in the trace, and one that runs 10 or more times is flagged as an N+1
/// query with this call site.
#[track_caller]
pub fn record_sql(sql: &str, duration: Duration, dialect: Option<Dialect>, rows: Option<u64>) {
    let location = Location::caller();
    let Some(recorder) = current_recorder() else { return };
    recorder.record_sql(sql, began(duration), duration, dialect, rows, None, SiteSource::Location(location));
}

/// Records an outgoing HTTP call that just ended (`status` is `None` when
/// there was no response).
pub fn record_http(method: &str, url: &str, status: Option<u16>, duration: Duration) {
    let Some(recorder) = current_recorder() else { return };
    recorder.record_http(method, url, status, began(duration), duration);
}

/// Starts timing a span; it is recorded when the timer is finished or
/// dropped. The timer remembers the request or job it started in, so it can
/// end on another thread.
///
/// ```
/// use goodsorcery::performance::{SpanKind, start_span};
/// let mut timer = start_span(SpanKind::Http, "POST https://api.example.com/v1/charges");
/// // … make the call …
/// timer.set_status(201);
/// timer.finish();
/// ```
#[track_caller]
pub fn start_span(kind: SpanKind, description: impl Into<String>) -> SpanTimer {
    SpanTimer {
        kind,
        description: description.into(),
        start: Instant::now(),
        location: Location::caller(),
        status: None,
        recorder: current_recorder(),
    }
}

/// A running span; see [`start_span`].
#[derive(Debug)]
#[must_use = "the span is recorded when the timer is dropped"]
pub struct SpanTimer {
    kind: SpanKind,
    description: String,
    start: Instant,
    location: &'static Location<'static>,
    status: Option<u16>,
    recorder: Option<Arc<Recorder>>,
}

impl SpanTimer {
    /// The HTTP response status, for an HTTP span.
    pub fn set_status(&mut self, status: u16) {
        self.status = Some(status);
    }

    /// Records the span now.
    pub fn finish(self) {}
}

impl Drop for SpanTimer {
    fn drop(&mut self) {
        if let Some(recorder) = self.recorder.take() {
            let duration = self.start.elapsed();
            record_into(&recorder, self.kind, &self.description, self.status, self.start, duration, self.location);
        }
    }
}

fn record_into(
    recorder: &Recorder,
    kind: SpanKind,
    description: &str,
    status: Option<u16>,
    start: Instant,
    duration: Duration,
    location: &'static Location<'static>,
) {
    match kind {
        SpanKind::Sql => {
            recorder.record_sql(description, start, duration, None, None, None, SiteSource::Location(location))
        }
        SpanKind::Http => {
            let (method, url) = split_http_description(description);
            recorder.record_http(method, url, status, start, duration);
        }
        SpanKind::View => recorder.record_view(description, start, duration),
        SpanKind::Enqueue => recorder.record_enqueue(description, None, start, duration),
    }
}

/// `"GET https://…"` → (`GET`, `https://…`); a bare URL has no method.
fn split_http_description(description: &str) -> (&str, &str) {
    let description = description.trim();
    match description.split_once(char::is_whitespace) {
        Some((method, url)) if !method.is_empty() && method.bytes().all(|b| b.is_ascii_alphabetic()) => {
            (method, url.trim())
        }
        _ => ("", description),
    }
}

/// Renames the current request or job (for example after routing that the
/// layer could not see).
pub fn set_transaction_name(name: impl Into<String>) {
    if let Some(recorder) = current_recorder() {
        lock(&recorder.data).name = truncate(&name.into(), 255);
    }
}

/// Whether the current thread is recording a request or job.
pub fn is_recording() -> bool {
    current_recorder().is_some()
}

/// When a span that just ended for `duration` began (never panics on an
/// out-of-range duration).
pub(crate) fn began(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_sub(duration).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_descriptions() {
        assert_eq!(split_http_description("GET https://a.b/c"), ("GET", "https://a.b/c"));
        assert_eq!(split_http_description("https://a.b/c"), ("", "https://a.b/c"));
    }
}

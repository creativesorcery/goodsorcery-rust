//! Traces: the slowest unit of work per transaction per minute.
//!
//! When a unit of work finishes, [`TraceBuffer::reserve`] decides cheaply
//! whether it is the slowest so far for its minute and transaction. If not,
//! its spans are dropped at once (the common case). If it is, its raw data
//! goes to the flusher thread, which builds the trace ([`build`]) off the
//! request path and stores it if nothing slower arrived meanwhile.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};

use super::aggregator::minute_iso;
use super::recorder::{CallSite, Detail, Kind, RecData};
use super::sql;
use crate::backtrace::{self, Paths, SourceCache};
use crate::runtime::RuntimeInfo;
use crate::util::{format_uuid, iso8601_millis, millis, round3, truncate, uuid_v7};

pub(crate) const MAX_TRACES_PER_MINUTE: usize = 30;
const MAX_SPANS: usize = 300;
const MAX_TRACE_BYTES: usize = 64 * 1024;

/// A finished unit of work that is the slowest so far in its minute.
#[derive(Debug)]
pub(crate) struct Candidate {
    pub(crate) seq: u64,
    pub(crate) minute: u64,
    pub(crate) started_at: SystemTime,
    pub(crate) duration: Duration,
    pub(crate) app: Duration,
    pub(crate) data: RecData,
}

#[derive(Debug)]
struct Slot {
    duration: Duration,
    seq: u64,
    trace: Option<Value>,
}

/// The traces held per minute, at most 30 per minute.
#[derive(Debug, Default)]
pub(crate) struct TraceBuffer {
    minutes: BTreeMap<u64, HashMap<(Kind, String), Slot>>,
    next_seq: u64,
}

impl TraceBuffer {
    /// Reserves a slot when `duration` is the slowest so far for the key;
    /// returns the candidate's sequence number.
    pub(crate) fn reserve(&mut self, minute: u64, kind: Kind, name: &str, duration: Duration) -> Option<u64> {
        let slots = self.minutes.entry(minute).or_default();
        let key = (kind, name.to_string());
        match slots.get(&key) {
            Some(slot) if slot.duration >= duration => return None,
            Some(_) => {}
            None if slots.len() >= MAX_TRACES_PER_MINUTE => {
                let fastest = slots
                    .iter()
                    .min_by_key(|(_, slot)| slot.duration)
                    .map(|(key, slot)| (key.clone(), slot.duration))?;
                if fastest.1 >= duration {
                    return None;
                }
                slots.remove(&fastest.0);
            }
            None => {}
        }
        self.next_seq += 1;
        let seq = self.next_seq;
        slots.insert(key, Slot { duration, seq, trace: None });
        while self.minutes.len() > super::aggregator::MAX_PENDING_MINUTES {
            self.minutes.pop_first();
        }
        Some(seq)
    }

    /// Stores a built trace if its slot was not taken by a slower one.
    pub(crate) fn fill(&mut self, minute: u64, kind: Kind, name: &str, seq: u64, trace: Value) {
        if let Some(slot) = self.minutes.get_mut(&minute).and_then(|slots| slots.get_mut(&(kind, name.to_string()))) {
            if slot.seq == seq {
                slot.trace = Some(trace);
            }
        }
    }

    /// Takes the built traces of minutes before `current` (or all),
    /// slowest first.
    pub(crate) fn take(&mut self, current: u64, include_open: bool) -> Vec<Value> {
        let keep = if include_open { BTreeMap::new() } else { self.minutes.split_off(&current) };
        let taken = std::mem::replace(&mut self.minutes, keep);
        let mut slots: Vec<Slot> =
            taken.into_values().flat_map(HashMap::into_values).filter(|slot| slot.trace.is_some()).collect();
        slots.sort_by_key(|slot| std::cmp::Reverse(slot.duration));
        slots.into_iter().filter_map(|slot| slot.trace).collect()
    }
}

/// What every trace in this process shares.
pub(crate) struct TraceContext {
    pub(crate) paths: Arc<Paths>,
    pub(crate) source: Option<SourceCache>,
    pub(crate) runtime: RuntimeInfo,
}

/// One entry of the trace's span list before it becomes JSON.
struct Entry {
    offset: Duration,
    last_end: Duration,
    duration: Duration,
    count: u32,
    rows: Option<u64>,
    kind: EntryKind,
}

enum EntryKind {
    Sql { sql: String, name: Option<String>, shape: u64 },
    Http { method: String, host: String, path: String, status: Option<u16> },
    View { template: String },
    Enqueue { job: String, queue: Option<String> },
}

/// Builds a trace payload (`INGEST_API.md` › Traces payload) from a
/// candidate.
pub(crate) fn build(candidate: &Candidate, context: &TraceContext) -> Value {
    let data = &candidate.data;
    let (mut spans, dropped) = span_entries(data, context);
    let mut trace = Map::new();
    trace.insert("id".into(), Value::String(format_uuid(&uuid_v7(candidate.started_at))));
    trace.insert("kind".into(), Value::String(data.kind.name().into()));
    trace.insert("name".into(), Value::String(truncate(&data.name, 255)));
    match data.kind {
        Kind::Web => {
            insert_opt(&mut trace, "method", &data.method);
            insert_opt(&mut trace, "route", &data.route);
        }
        Kind::Job => {
            insert_opt(&mut trace, "adapter", &data.adapter);
            insert_opt(&mut trace, "queue", &data.queue);
        }
    }
    trace.insert("minute".into(), Value::String(minute_iso(candidate.minute)));
    trace.insert("started_at".into(), Value::String(iso8601_millis(candidate.started_at)));
    trace.insert("duration_ms".into(), json!(round3(millis(candidate.duration))));
    trace.insert("db_ms".into(), json!(round3(millis(data.db))));
    trace.insert("http_ms".into(), json!(round3(millis(data.http))));
    trace.insert("view_ms".into(), json!(round3(millis(data.view))));
    trace.insert("app_ms".into(), json!(round3(millis(candidate.app))));
    if data.kind == Kind::Web {
        if let Some(status) = data.status {
            trace.insert("status".into(), Value::from(status));
        }
    }
    trace.insert("failed".into(), Value::Bool(data.failed));
    trace.insert("error_event_id".into(), data.error_event_id.map_or(Value::Null, |id| Value::String(id.to_string())));
    match data.kind {
        Kind::Web => {
            let mut request = Map::new();
            insert_opt(&mut request, "method", &data.method);
            insert_opt(&mut request, "path", &data.path.as_ref().map(|path| truncate(path, 1_024)));
            insert_opt(&mut request, "request_id", &data.request_id.as_ref().map(|id| truncate(id, 128)));
            trace.insert("request".into(), Value::Object(request));
        }
        Kind::Job => {
            let mut job = Map::new();
            insert_opt(&mut job, "adapter", &data.adapter);
            insert_opt(&mut job, "queue", &data.queue);
            if let Some(wait) = data.queue_wait {
                job.insert("queue_wait_ms".into(), json!(round3(millis(wait))));
            }
            if let Some(executions) = data.executions {
                job.insert("executions".into(), Value::from(executions));
            }
            trace.insert("job".into(), Value::Object(job));
        }
    }
    if let Some(host) = &context.runtime.host {
        trace.insert("host".into(), Value::String(host.clone()));
    }
    trace.insert("process".into(), Value::String(context.runtime.trace_process()));
    trace.insert("runtime".into(), context.runtime.runtime_list());
    trace.insert("sql_count".into(), Value::from(data.sql_count));
    trace.insert("http_count".into(), Value::from(data.http_count));
    let mut dropped = dropped + data.spans_dropped as usize;
    fit(&trace, &mut spans, &mut dropped);
    trace.insert("spans_dropped".into(), Value::from(dropped));
    trace.insert("spans".into(), Value::Array(spans.into_iter().map(|(_, span)| span).collect()));
    Value::Object(trace)
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: &Option<String>) {
    if let Some(value) = value {
        map.insert(key.into(), Value::String(value.clone()));
    }
}

/// Normalises and groups the raw spans, applies the span cap and turns them
/// into JSON. Returns (duration, span) pairs in start order and the number
/// dropped by the cap.
fn span_entries(data: &RecData, context: &TraceContext) -> (Vec<(Duration, Value)>, usize) {
    let mut order: Vec<usize> = (0..data.spans.len()).collect();
    order.sort_by_key(|&index| data.spans[index].offset);
    let mut entries: Vec<Entry> = Vec::new();
    let mut groups: HashMap<String, usize> = HashMap::new();
    let mut memo: HashMap<(&str, Option<sql::Dialect>), String> = HashMap::new();
    for index in order {
        let raw = &data.spans[index];
        let end = raw.offset + raw.duration;
        let (group_key, kind, rows) = match &raw.detail {
            Detail::Sql { sql: text, dialect, rows, name, shape } => {
                let normalized =
                    memo.entry((text.as_str(), *dialect)).or_insert_with(|| sql::normalize(text, *dialect)).clone();
                (
                    Some(format!("sql:{normalized}")),
                    EntryKind::Sql { sql: normalized, name: name.clone(), shape: *shape },
                    *rows,
                )
            }
            Detail::View { template } => {
                (Some(format!("view:{template}")), EntryKind::View { template: template.clone() }, None)
            }
            Detail::Http { method, host, path, status } => (
                None,
                EntryKind::Http { method: method.clone(), host: host.clone(), path: path.clone(), status: *status },
                None,
            ),
            Detail::Enqueue { job, queue } => {
                (None, EntryKind::Enqueue { job: job.clone(), queue: queue.clone() }, None)
            }
        };
        if let Some(position) = group_key.as_ref().and_then(|key| groups.get(key)) {
            let entry = &mut entries[*position];
            entry.count += 1;
            entry.duration += raw.duration;
            entry.last_end = entry.last_end.max(end);
            entry.rows = match (entry.rows, rows) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            };
            continue;
        }
        if let Some(key) = group_key {
            groups.insert(key, entries.len());
        }
        entries.push(Entry { offset: raw.offset, last_end: end, duration: raw.duration, count: 1, rows, kind });
    }
    let mut dropped = 0;
    if entries.len() > MAX_SPANS {
        dropped = entries.len() - MAX_SPANS;
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.duration));
        entries.truncate(MAX_SPANS);
        entries.sort_by_key(|entry| entry.offset);
    }
    let spans = entries.iter().map(|entry| (entry.duration, entry_json(entry, data, context))).collect();
    (spans, dropped)
}

fn entry_json(entry: &Entry, data: &RecData, context: &TraceContext) -> Value {
    let mut span = Map::new();
    let kind = match entry.kind {
        EntryKind::Sql { .. } => "sql",
        EntryKind::Http { .. } => "http",
        EntryKind::View { .. } => "view",
        EntryKind::Enqueue { .. } => "enqueue",
    };
    span.insert("type".into(), Value::String(kind.into()));
    span.insert("offset_ms".into(), json!(round3(millis(entry.offset))));
    span.insert("duration_ms".into(), json!(round3(millis(entry.duration))));
    if entry.count >= 2 {
        span.insert("count".into(), Value::from(entry.count));
        span.insert("span_ms".into(), json!(round3(millis(entry.last_end.saturating_sub(entry.offset)))));
    }
    match &entry.kind {
        EntryKind::Sql { sql, name, shape } => {
            span.insert("sql".into(), Value::String(sql.clone()));
            if let Some(name) = name {
                span.insert("name".into(), Value::String(name.clone()));
            }
            if let Some(rows) = entry.rows {
                span.insert("rows".into(), Value::from(rows));
            }
            if entry.count >= 2 {
                if let Some(site) = data.call_sites.get(shape).and_then(|site| call_site(site, context)) {
                    span.insert("call_site".into(), site);
                }
            }
        }
        EntryKind::Http { method, host, path, status } => {
            if !method.is_empty() {
                span.insert("method".into(), Value::String(method.clone()));
            }
            span.insert("host".into(), Value::String(host.clone()));
            span.insert("path".into(), Value::String(path.clone()));
            if let Some(status) = status {
                span.insert("status".into(), Value::from(*status));
            }
        }
        EntryKind::View { template } => {
            span.insert("template".into(), Value::String(template.clone()));
        }
        EntryKind::Enqueue { job, queue } => {
            span.insert("job".into(), Value::String(job.clone()));
            if let Some(queue) = queue {
                span.insert("queue".into(), Value::String(queue.clone()));
            }
        }
    }
    Value::Object(span)
}

/// A repeated query's call site: app-relative file, line, method and, with
/// code context on, two lines either side.
fn call_site(site: &CallSite, context: &TraceContext) -> Option<Value> {
    let (file, line, method) = match site {
        CallSite::Location(location) => (location.file().to_string(), location.line(), None),
        CallSite::Static { file, line } => (file.to_string(), *line, None),
        CallSite::Backtrace(trace) => {
            let frame = backtrace::first_app_frame(trace, &context.paths)?;
            let method = frame.symbol.as_deref().map(|symbol| backtrace::split_symbol(symbol).1);
            (frame.file?, frame.line?, method)
        }
    };
    let mut map = Map::new();
    map.insert("file".into(), Value::String(backtrace::display_path(&context.paths, &file)));
    map.insert("line".into(), Value::from(line));
    if let Some(method) = method {
        map.insert("method".into(), Value::String(truncate(&method, 255)));
    }
    if let Some(source) = &context.source {
        if let Some(lines) = source.context(&file, line, 2, 2, 200) {
            map.insert("context".into(), lines);
        }
    }
    Some(Value::Object(map))
}

/// Keeps the trace under 64 KiB by dropping its shortest spans, half at a
/// time.
fn fit(trace: &Map<String, Value>, spans: &mut Vec<(Duration, Value)>, dropped: &mut usize) {
    let base = serde_json::to_vec(trace).map_or(0, |json| json.len()) + 64;
    loop {
        let size: usize = base
            + spans.iter().map(|(_, span)| serde_json::to_vec(span).map_or(0, |json| json.len() + 1)).sum::<usize>();
        if size <= MAX_TRACE_BYTES || spans.is_empty() {
            return;
        }
        let remove = spans.len().div_ceil(2);
        let mut by_duration: Vec<usize> = (0..spans.len()).collect();
        by_duration.sort_by_key(|&index| spans[index].0);
        let shortest: std::collections::HashSet<usize> = by_duration.into_iter().take(remove).collect();
        let mut index = 0;
        spans.retain(|_| {
            let keep = !shortest.contains(&index);
            index += 1;
            keep
        });
        *dropped += remove;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::performance::recorder::RawSpan;

    fn context() -> TraceContext {
        TraceContext {
            paths: Arc::new(Paths::new(None, vec![], vec![], None, None)),
            source: Some(SourceCache::new()),
            runtime: RuntimeInfo::new(&Config::new().server_name("web-2")),
        }
    }

    fn sql_span(offset_ms: u64, duration_ms: u64, text: &str) -> RawSpan {
        RawSpan {
            offset: Duration::from_millis(offset_ms),
            duration: Duration::from_millis(duration_ms),
            detail: Detail::Sql { sql: text.into(), dialect: None, rows: Some(1), name: None, shape: sql::shape(text) },
        }
    }

    #[test]
    fn reserves_only_the_slowest() {
        let mut buffer = TraceBuffer::default();
        let first = buffer.reserve(60, Kind::Web, "GET /a", Duration::from_millis(100)).unwrap();
        assert!(buffer.reserve(60, Kind::Web, "GET /a", Duration::from_millis(50)).is_none());
        let second = buffer.reserve(60, Kind::Web, "GET /a", Duration::from_millis(200)).unwrap();
        buffer.fill(60, Kind::Web, "GET /a", first, json!("stale"));
        buffer.fill(60, Kind::Web, "GET /a", second, json!("slowest"));
        assert!(buffer.take(60, false).is_empty());
        assert_eq!(buffer.take(120, false), vec![json!("slowest")]);
    }

    #[test]
    fn holds_at_most_thirty_traces_per_minute() {
        let mut buffer = TraceBuffer::default();
        for n in 0..MAX_TRACES_PER_MINUTE as u64 {
            let seq = buffer.reserve(60, Kind::Web, &format!("t{n}"), Duration::from_millis(10 + n)).unwrap();
            buffer.fill(60, Kind::Web, &format!("t{n}"), seq, json!(n));
        }
        assert!(buffer.reserve(60, Kind::Web, "new", Duration::from_millis(5)).is_none());
        let seq = buffer.reserve(60, Kind::Web, "new", Duration::from_millis(500)).unwrap();
        buffer.fill(60, Kind::Web, "new", seq, json!("new"));
        let traces = buffer.take(120, true);
        assert_eq!(traces.len(), MAX_TRACES_PER_MINUTE);
        assert_eq!(traces[0], json!("new"));
        assert!(!traces.contains(&json!(0)));
    }

    #[test]
    fn groups_repeated_queries_and_strips_literals() {
        let mut data = RecData::new(Kind::Web, "GET /cart".into());
        data.method = Some("GET".into());
        data.route = Some("/cart".into());
        data.path = Some("/cart".into());
        data.status = Some(200);
        data.spans.push(sql_span(0, 3, "SELECT * FROM users WHERE email = 'mara@example.com'"));
        for n in 0..12u64 {
            data.spans.push(sql_span(10 + n * 5, 2, &format!("SELECT * FROM products WHERE id = {n}")));
        }
        let shape = sql::shape("SELECT * FROM products WHERE id = 1");
        data.call_sites.insert(shape, CallSite::Location(std::panic::Location::caller()));
        data.db = Duration::from_millis(27);
        data.sql_count = 13;
        let candidate = Candidate {
            seq: 1,
            minute: 1_793_802_720,
            started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_793_802_728),
            duration: Duration::from_millis(100),
            app: Duration::from_millis(73),
            data,
        };
        let trace = build(&candidate, &context());
        assert_eq!(trace["kind"], "web");
        assert_eq!(trace["minute"], "2026-11-04T14:32:00Z");
        assert_eq!(trace["started_at"], "2026-11-04T14:32:08.000Z");
        assert_eq!(trace["db_ms"], 27.0);
        assert_eq!(trace["app_ms"], 73.0);
        assert_eq!(trace["request"], json!({ "method": "GET", "path": "/cart" }));
        assert_eq!(trace["host"], "web-2");
        assert_eq!(trace["error_event_id"], Value::Null);
        assert!(trace["id"].as_str().unwrap().starts_with(&format_uuid(&uuid_v7(candidate.started_at))[..13]));
        let spans = trace["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["sql"], "SELECT * FROM users WHERE email = ?");
        assert_eq!(spans[1]["sql"], "SELECT * FROM products WHERE id = ?");
        assert_eq!(spans[1]["count"], 12);
        assert_eq!(spans[1]["duration_ms"], 24.0);
        assert_eq!(spans[1]["span_ms"], 57.0);
        assert_eq!(spans[1]["rows"], 12);
        assert_eq!(spans[1]["call_site"]["file"], "src/performance/traces.rs");
        assert!(spans[1]["call_site"]["context"]["lines"].as_array().unwrap().len() == 5);
        assert!(!trace.to_string().contains("mara@example.com"));
    }

    #[test]
    fn caps_spans_and_counts_the_rest() {
        let mut data = RecData::new(Kind::Job, "Import".into());
        for n in 0..400u64 {
            data.spans.push(RawSpan {
                offset: Duration::from_millis(n),
                duration: Duration::from_micros(n + 1),
                detail: Detail::Http {
                    method: "GET".into(),
                    host: "api".into(),
                    path: format!("/p/{n}"),
                    status: Some(200),
                },
            });
        }
        data.spans_dropped = 5;
        let candidate = Candidate {
            seq: 1,
            minute: 60,
            started_at: SystemTime::now(),
            duration: Duration::from_secs(1),
            app: Duration::ZERO,
            data,
        };
        let trace = build(&candidate, &context());
        assert_eq!(trace["spans"].as_array().unwrap().len(), MAX_SPANS);
        assert_eq!(trace["spans_dropped"], 105);
        assert_eq!(trace["spans"][0]["path"], "/p/100");
        assert!(trace.get("status").is_none());
        assert_eq!(trace["job"], json!({}));
    }
}

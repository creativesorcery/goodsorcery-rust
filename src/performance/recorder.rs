//! The per-request (or per-job-run) recorder: layer totals, the raw span
//! list and the call sites of repeated queries.
//!
//! Recording a span is a short lock and a vector push. Raw SQL is kept only
//! in memory while the unit of work runs (at most 2,000 spans and 1 MiB of
//! SQL text); past either bound spans are only counted, while the layer
//! totals stay exact. Normalisation and grouping happen later, off the
//! request path, and only for the slowest unit of work in its minute.

use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::panic::Location;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use super::Shared;
use super::sql::{self, Dialect};
use crate::event::EventId;
use crate::util::{lock, truncate};

pub(crate) const MAX_RAW_SPANS: usize = 2_000;
const MAX_SQL_BYTES: usize = 1024 * 1024;
const MAX_SHAPES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Kind {
    Web,
    Job,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Web => "web",
            Kind::Job => "job",
        }
    }
}

/// Where a repeated query was issued from.
#[derive(Debug)]
pub(crate) enum CallSite {
    /// From `#[track_caller]` (the public recording API).
    Location(&'static Location<'static>),
    /// Where an instrumented span was declared.
    Static { file: &'static str, line: u32 },
    /// A backtrace taken the second time the query ran, resolved only if
    /// the trace is built.
    Backtrace(Backtrace),
}

/// How to find a call site when one is needed.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SiteSource {
    Location(&'static Location<'static>),
    Static { file: &'static str, line: u32 },
    Capture,
}

#[derive(Debug)]
pub(crate) enum Detail {
    Sql { sql: String, dialect: Option<Dialect>, rows: Option<u64>, name: Option<String>, shape: u64 },
    Http { method: String, host: String, path: String, status: Option<u16> },
    View { template: String },
    Enqueue { job: String, queue: Option<String> },
}

#[derive(Debug)]
pub(crate) struct RawSpan {
    pub(crate) offset: Duration,
    pub(crate) duration: Duration,
    pub(crate) detail: Detail,
}

/// What a recorder holds.
#[derive(Debug)]
pub(crate) struct RecData {
    pub(crate) kind: Kind,
    pub(crate) name: String,
    pub(crate) method: Option<String>,
    pub(crate) route: Option<String>,
    pub(crate) adapter: Option<String>,
    pub(crate) queue: Option<String>,
    pub(crate) queue_wait: Option<Duration>,
    pub(crate) executions: Option<u32>,
    pub(crate) path: Option<String>,
    pub(crate) request_id: Option<String>,
    pub(crate) status: Option<u16>,
    pub(crate) failed: bool,
    pub(crate) error_event_id: Option<EventId>,
    pub(crate) db: Duration,
    pub(crate) http: Duration,
    pub(crate) view: Duration,
    pub(crate) spans: Vec<RawSpan>,
    pub(crate) spans_dropped: u32,
    pub(crate) sql_bytes: usize,
    pub(crate) sql_count: u32,
    pub(crate) http_count: u32,
    pub(crate) shapes: HashMap<u64, u32>,
    pub(crate) call_sites: HashMap<u64, CallSite>,
    pub(crate) finished: bool,
}

impl RecData {
    pub(crate) fn new(kind: Kind, name: String) -> Self {
        RecData {
            kind,
            name,
            method: None,
            route: None,
            adapter: None,
            queue: None,
            queue_wait: None,
            executions: None,
            path: None,
            request_id: None,
            status: None,
            failed: false,
            error_event_id: None,
            db: Duration::ZERO,
            http: Duration::ZERO,
            view: Duration::ZERO,
            spans: Vec::new(),
            spans_dropped: 0,
            sql_bytes: 0,
            sql_count: 0,
            http_count: 0,
            shapes: HashMap::new(),
            call_sites: HashMap::new(),
            finished: false,
        }
    }

    fn push(&mut self, span: RawSpan) {
        if self.spans.len() < MAX_RAW_SPANS {
            self.spans.push(span);
        } else {
            self.spans_dropped += 1;
        }
    }
}

/// One unit of work being recorded.
pub(crate) struct Recorder {
    pub(crate) started: Instant,
    pub(crate) started_at: SystemTime,
    pub(crate) shared: Arc<Shared>,
    pub(crate) data: Mutex<RecData>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder").field("started_at", &self.started_at).finish_non_exhaustive()
    }
}

impl Recorder {
    pub(crate) fn new(shared: Arc<Shared>, data: RecData) -> Self {
        Recorder { started: Instant::now(), started_at: SystemTime::now(), shared, data: Mutex::new(data) }
    }

    fn offset(&self, start: Instant) -> Duration {
        start.saturating_duration_since(self.started)
    }

    /// Records a query that started at `start` and took `duration`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_sql(
        &self,
        statement: &str,
        start: Instant,
        duration: Duration,
        dialect: Option<Dialect>,
        rows: Option<u64>,
        name: Option<&str>,
        site: SiteSource,
    ) {
        let shape = sql::shape(statement);
        let dialect = dialect.or(self.shared.sql_dialect);
        let need_site = {
            let mut data = lock(&self.data);
            if data.finished {
                return;
            }
            data.db += duration;
            data.sql_count += 1;
            let shapes = data.shapes.len();
            let seen = match data.shapes.get_mut(&shape) {
                Some(count) => {
                    *count += 1;
                    *count
                }
                None if shapes < MAX_SHAPES => {
                    data.shapes.insert(shape, 1);
                    1
                }
                None => 0,
            };
            if data.spans.len() < MAX_RAW_SPANS && data.sql_bytes < MAX_SQL_BYTES {
                let text = truncate(statement, 4_001);
                data.sql_bytes += text.len();
                let name = name.map(|name| truncate(name, 255));
                data.push(RawSpan {
                    offset: self.offset(start),
                    duration,
                    detail: Detail::Sql { sql: text, dialect, rows, name, shape },
                });
            } else {
                data.spans_dropped += 1;
            }
            seen == 2 && !data.call_sites.contains_key(&shape)
        };
        if need_site {
            let site = match site {
                SiteSource::Location(location) => CallSite::Location(location),
                SiteSource::Static { file, line } => CallSite::Static { file, line },
                SiteSource::Capture => CallSite::Backtrace(Backtrace::force_capture()),
            };
            lock(&self.data).call_sites.entry(shape).or_insert(site);
        }
    }

    /// Records an outgoing HTTP call: method, host and path only.
    pub(crate) fn record_http(&self, method: &str, url: &str, status: Option<u16>, start: Instant, duration: Duration) {
        let (host, path) = split_url(url);
        let mut data = lock(&self.data);
        if data.finished {
            return;
        }
        data.http += duration;
        data.http_count += 1;
        data.push(RawSpan {
            offset: self.offset(start),
            duration,
            detail: Detail::Http { method: truncate(&method.to_ascii_uppercase(), 16), host, path, status },
        });
    }

    /// Records rendering of a template or view.
    pub(crate) fn record_view(&self, template: &str, start: Instant, duration: Duration) {
        let mut data = lock(&self.data);
        if data.finished {
            return;
        }
        data.view += duration;
        data.push(RawSpan {
            offset: self.offset(start),
            duration,
            detail: Detail::View { template: truncate(template, 255) },
        });
    }

    /// Records a job being enqueued.
    pub(crate) fn record_enqueue(&self, job: &str, queue: Option<&str>, start: Instant, duration: Duration) {
        let mut data = lock(&self.data);
        if data.finished {
            return;
        }
        data.push(RawSpan {
            offset: self.offset(start),
            duration,
            detail: Detail::Enqueue { job: truncate(job, 255), queue: queue.map(|queue| truncate(queue, 128)) },
        });
    }
}

/// Splits a URL (or a bare path) into a host without credentials and a path
/// without query string or fragment.
///
/// Credentials go first: everything up to the last `@` before the first
/// `/` is dropped, so a password holding an unencoded `?` or `#` never
/// shows up as the host. What is left of the authority must look like a
/// host (`name`, `name:port` or `[v6]:port`); anything else (a password
/// with an unencoded `/`, say) sends no host and path `/`, never a part of
/// the credentials.
pub(crate) fn split_url(url: &str) -> (String, String) {
    let url = url.trim();
    let Some((_, after)) = url.split_once("://") else {
        let path = url.split(['?', '#']).next().unwrap_or("");
        return (String::new(), truncate(if path.is_empty() { "/" } else { path }, 1_024));
    };
    let slash = after.find('/').unwrap_or(after.len());
    let (authority, rest) = after.split_at(slash);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (host, path) = match authority.find(['?', '#']) {
        // The query or fragment starts before any path.
        Some(end) => (&authority[..end], "/"),
        None => (authority, rest.split(['?', '#']).next().unwrap_or("")),
    };
    if !looks_like_host(host) {
        return (String::new(), "/".to_string());
    }
    let path = if path.is_empty() { "/" } else { path };
    (truncate(host, 255), truncate(path, 1_024))
}

/// `name`, `name:port`, `[v6]` or `[v6]:port` (an empty host is fine).
fn looks_like_host(host: &str) -> bool {
    let port = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((_, "")) => None,
            Some((_, after)) => Some(after.strip_prefix(':').unwrap_or("x")),
            None => return false,
        },
        None => host.split_once(':').map(|(_, port)| port),
    };
    port.is_none_or(|port| port.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_credentials_queries_and_fragments() {
        assert_eq!(
            split_url("https://user:pass@api.taxjar.com:443/v2/rates/94107?api_key=secret#x"),
            ("api.taxjar.com:443".to_string(), "/v2/rates/94107".to_string())
        );
        assert_eq!(split_url("http://example.com"), ("example.com".to_string(), "/".to_string()));
        assert_eq!(split_url("/local/path?q=1"), (String::new(), "/local/path".to_string()));
        assert_eq!(split_url("http://[::1]:8080/x"), ("[::1]:8080".to_string(), "/x".to_string()));
        assert_eq!(split_url("https://api.example.com?q=1"), ("api.example.com".to_string(), "/".to_string()));
    }

    #[test]
    fn credentials_with_unencoded_delimiters_never_leak() {
        assert_eq!(
            split_url("https://user:pa?ss@api.example.com/x"),
            ("api.example.com".to_string(), "/x".to_string())
        );
        assert_eq!(
            split_url("https://user:pa#ss@api.example.com/x"),
            ("api.example.com".to_string(), "/x".to_string())
        );
        // A `/` in the password: the authority can't be told apart, so
        // neither host nor path is sent.
        assert_eq!(split_url("https://user:pa/ss@api.example.com/x"), (String::new(), "/".to_string()));
        assert_eq!(split_url("https://user:secret/x"), (String::new(), "/".to_string()));
    }
}

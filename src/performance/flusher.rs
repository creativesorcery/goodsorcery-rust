//! The `goodsorcery-apm` thread: builds traces off the request path and
//! sends the closed minutes once a minute.
//!
//! It wakes at each minute boundary plus a per-process random offset of
//! 2–20 seconds (spreading the server's load), and sends one metrics
//! payload and, when there are traces, one traces payload. Each payload
//! gets a UUIDv7 `batch_id` when it is built and keeps it on every retry
//! (the server counts a batch once). 429 waits for `Retry-After`; 5xx and
//! network errors keep the payload for the next wake; a payload still
//! unsent after 3 attempts or 5 minutes is dropped. 400, 401, 413, 415 and
//! 422 drop it. 404 means the server has no performance endpoints, so
//! performance monitoring turns off for the process. On shutdown it sends
//! every minute, the open one included (the server adds metrics up, so a
//! partial minute is safe).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use super::Shared;
use super::aggregator::{TransactionStats, minute_iso};
use super::histogram::SCHEME;
use super::traces::{self, Candidate, TraceContext};
use crate::runtime::notifier;
use crate::transport::{METRICS_PATH, Outcome, TRACES_PATH, Transport};
use crate::util::{lock, log, minute_of, new_uuid, random_unit, unix_seconds};

const MAX_METRICS_BYTES: usize = 450 * 1024;
const MAX_TRACES_BYTES: usize = 1_800 * 1024;
const MAX_TRACES_PER_BODY: usize = 50;
const MAX_TRANSACTIONS_PER_MINUTE: usize = 1_000;
const MAX_MINUTES_PER_BODY: usize = 5;
const MAX_ATTEMPTS: u32 = 3;
const MAX_AGE: Duration = Duration::from_secs(300);
const MAX_PENDING: usize = 20;
pub(crate) const MAX_BUILDS: usize = 64;

/// What the flusher waits on.
#[derive(Debug, Default)]
pub(crate) struct FlushState {
    pub(crate) builds: VecDeque<Candidate>,
    pub(crate) requested: u64,
    pub(crate) flushed: u64,
    pub(crate) closing: bool,
    pub(crate) stopped: bool,
}

struct Payload {
    path: &'static str,
    body: Vec<u8>,
    attempts: u32,
    created: Instant,
    not_before: Option<Instant>,
}

pub(crate) struct Flusher {
    pub(crate) shared: Arc<Shared>,
    pub(crate) transport: Arc<Transport>,
    pub(crate) context: TraceContext,
    pending: VecDeque<Payload>,
    offset: f64,
}

impl Flusher {
    pub(crate) fn new(shared: Arc<Shared>, transport: Arc<Transport>, context: TraceContext) -> Self {
        Flusher { shared, transport, context, pending: VecDeque::new(), offset: 2.0 + random_unit() * 18.0 }
    }

    /// The next wake: the minute boundary plus this process's offset.
    fn next_wake(&self) -> Instant {
        let now = unix_seconds(SystemTime::now());
        let mut at = minute_of(SystemTime::now()) as f64 + self.offset;
        if at <= now {
            at += 60.0;
        }
        Instant::now() + Duration::from_secs_f64((at - now).max(0.0))
    }

    pub(crate) fn run(mut self) {
        let mut wake = self.next_wake();
        loop {
            let (builds, flush_to, closing, due) = {
                let mut state = lock(&self.shared.flush);
                loop {
                    let now = Instant::now();
                    if !state.builds.is_empty() || state.requested > state.flushed || state.closing || now >= wake {
                        break;
                    }
                    state = self
                        .shared
                        .flush_signal
                        .wait_timeout(state, wake - now)
                        .map(|(guard, _)| guard)
                        .unwrap_or_else(|e| e.into_inner().0);
                }
                let builds: Vec<Candidate> = state.builds.drain(..).collect();
                let flush_to = (state.requested > state.flushed).then_some(state.requested);
                (builds, flush_to, state.closing, Instant::now() >= wake)
            };
            for candidate in builds {
                self.build(candidate);
            }
            if closing || flush_to.is_some() {
                self.flush(true);
            } else if due {
                self.flush(false);
            }
            if due {
                wake = self.next_wake();
            }
            let mut state = lock(&self.shared.flush);
            if let Some(generation) = flush_to {
                state.flushed = state.flushed.max(generation);
            }
            if closing {
                state.stopped = true;
                state.flushed = state.requested;
            }
            self.shared.flush_done.notify_all();
            if closing {
                break;
            }
        }
    }

    fn build(&self, candidate: Candidate) {
        match crate::event::guard(|| traces::build(&candidate, &self.context)) {
            Some(trace) => lock(&self.shared.traces).fill(
                candidate.minute,
                candidate.data.kind,
                &candidate.data.name,
                candidate.seq,
                trace,
            ),
            None => {
                log::warn_every("trace_build", Duration::from_secs(60), "a trace could not be built; it was skipped")
            }
        }
    }

    /// Takes closed minutes (or all) and sends them with anything pending.
    fn flush(&mut self, include_open: bool) {
        let current = minute_of(SystemTime::now());
        let minutes = lock(&self.shared.aggregator).take(current, include_open);
        let traces = lock(&self.shared.traces).take(current, include_open);
        let now = Instant::now();
        for body in metrics_bodies(minutes) {
            self.pending.push_back(Payload { path: METRICS_PATH, body, attempts: 0, created: now, not_before: None });
        }
        for body in trace_bodies(traces) {
            self.pending.push_back(Payload { path: TRACES_PATH, body, attempts: 0, created: now, not_before: None });
        }
        while self.pending.len() > MAX_PENDING {
            self.pending.pop_front();
            self.dropped("too many payloads waiting");
        }
        self.send_pending();
    }

    fn send_pending(&mut self) {
        let mut keep = VecDeque::new();
        let mut stop = false;
        while let Some(mut payload) = self.pending.pop_front() {
            let now = Instant::now();
            if stop || payload.not_before.is_some_and(|at| at > now) || !self.shared.on.load(Ordering::Relaxed) {
                if self.shared.on.load(Ordering::Relaxed) {
                    keep.push_back(payload);
                }
                continue;
            }
            match self.transport.post(payload.path, &payload.body) {
                Outcome::Success(_) => {}
                Outcome::NotFound => {
                    log::warn_once(
                        "apm_not_found",
                        "the server has no performance endpoints; performance monitoring is off",
                    );
                    self.shared.on.store(false, Ordering::Relaxed);
                    keep.clear();
                    stop = true;
                }
                Outcome::RateLimited(wait) => {
                    payload.not_before = Some(now + wait);
                    keep.push_back(payload);
                    stop = true;
                }
                Outcome::Retryable(reason) => {
                    payload.attempts += 1;
                    if payload.attempts >= MAX_ATTEMPTS || payload.created.elapsed() > MAX_AGE {
                        self.dropped(&reason);
                    } else {
                        keep.push_back(payload);
                    }
                    stop = true;
                }
                Outcome::Rejected(status) => self.dropped(&format!("HTTP {status}")),
            }
        }
        keep.retain(|payload| {
            let fresh = payload.created.elapsed() <= MAX_AGE;
            if !fresh {
                log::warn_every(
                    "apm_dropped",
                    Duration::from_secs(60),
                    "a performance payload was dropped (unsent for 5 minutes)",
                );
            }
            fresh
        });
        self.pending = keep;
    }

    fn dropped(&self, reason: &str) {
        log::warn_every(
            "apm_dropped",
            Duration::from_secs(60),
            format!("a performance payload was dropped ({reason})"),
        );
    }
}

/// Metrics bodies of at most 5 minutes, 1,000 transactions per minute and
/// about 450 KiB each (each with its own `batch_id`; the server adds them
/// up).
pub(crate) fn metrics_bodies(minutes: Vec<(u64, Vec<TransactionStats>)>) -> Vec<Vec<u8>> {
    let mut bodies = Vec::new();
    let mut current: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
    let mut size = 0;
    for (minute, list) in minutes {
        for stats in list {
            let json = stats.to_json();
            let bytes = serde_json::to_vec(&json).map_or(0, |b| b.len()) + 1;
            let full = size + bytes > MAX_METRICS_BYTES
                || (!current.contains_key(&minute) && current.len() >= MAX_MINUTES_PER_BODY)
                || current.get(&minute).is_some_and(|list| list.len() >= MAX_TRANSACTIONS_PER_MINUTE);
            if full && !current.is_empty() {
                bodies.push(metrics_body(std::mem::take(&mut current)));
                size = 0;
            }
            size += bytes;
            current.entry(minute).or_default().push(json);
        }
    }
    if !current.is_empty() {
        bodies.push(metrics_body(current));
    }
    bodies
}

fn metrics_body(minutes: BTreeMap<u64, Vec<Value>>) -> Vec<u8> {
    let minutes: Vec<Value> = minutes
        .into_iter()
        .map(|(minute, transactions)| json!({ "minute": minute_iso(minute), "transactions": transactions }))
        .collect();
    let body = json!({ "batch_id": new_uuid(), "histogram": SCHEME, "notifier": notifier(), "minutes": minutes });
    serde_json::to_vec(&body).unwrap_or_default()
}

/// Trace bodies of at most 50 traces and about 1.8 MiB each.
pub(crate) fn trace_bodies(traces: Vec<Value>) -> Vec<Vec<u8>> {
    let mut bodies = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    let mut size = 0;
    for trace in traces {
        let bytes = serde_json::to_vec(&trace).map_or(0, |b| b.len()) + 1;
        if !current.is_empty() && (current.len() >= MAX_TRACES_PER_BODY || size + bytes > MAX_TRACES_BYTES) {
            bodies.push(traces_body(std::mem::take(&mut current)));
            size = 0;
        }
        size += bytes;
        current.push(trace);
    }
    if !current.is_empty() {
        bodies.push(traces_body(current));
    }
    bodies
}

fn traces_body(traces: Vec<Value>) -> Vec<u8> {
    let body = json!({ "batch_id": new_uuid(), "notifier": notifier(), "traces": traces });
    serde_json::to_vec(&body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_trace_bodies_at_fifty() {
        let traces: Vec<Value> = (0..120).map(|n| json!({ "n": n })).collect();
        let bodies = trace_bodies(traces);
        assert_eq!(bodies.len(), 3);
        let first: Value = serde_json::from_slice(&bodies[0]).unwrap();
        assert_eq!(first["traces"].as_array().unwrap().len(), 50);
        assert_eq!(first["notifier"]["name"], "goodsorcery-rust");
        let second: Value = serde_json::from_slice(&bodies[1]).unwrap();
        assert_ne!(first["batch_id"], second["batch_id"]);
    }
}

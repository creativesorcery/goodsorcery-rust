//! The bounded event queue and the `goodsorcery-worker` thread that sends
//! it in batches.
//!
//! Callers only push onto the queue (a short lock, no I/O). The worker
//! resolves backtraces, applies `before_send`, and posts up to `batch_size`
//! events to `/api/v1/events/batch` when that many are waiting or
//! `flush_interval` has passed. It retries 5xx and network failures with
//! exponential backoff and jitter, waits out 429s without spending a retry,
//! splits a batch refused as too large (413), falls back to one request per
//! event on a server without the batch endpoint (404), and drops what the
//! server rejects (400, 401, 415, 422). Nothing here panics into the app:
//! every step that runs app code or parses data is guarded.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::backtrace::{self, Paths, SourceCache};
use crate::config::Config;
use crate::event::{PendingEvent, guard};
use crate::transport::{BATCH_PATH, EVENTS_PATH, Outcome, Transport};
use crate::util::{lock, log, random_unit};

const MAX_EVENT_BYTES: usize = 512 * 1024;
const MAX_BATCH_BYTES: usize = 2 * 1024 * 1024 - 16 * 1024;
const MAX_BACKOFF: f64 = 60.0;

#[derive(Default)]
struct State {
    events: VecDeque<PendingEvent>,
    in_flight: usize,
    closed: bool,
    flush_requested: bool,
    dropped: u64,
}

/// The queue shared by callers and the worker.
pub(crate) struct Queue {
    state: Mutex<State>,
    /// Wakes the worker (new events, flush, close).
    wake: Condvar,
    /// Wakes flushers when the queue drains.
    idle: Condvar,
    max_size: usize,
}

impl Queue {
    fn new(max_size: usize) -> Self {
        Queue { state: Mutex::new(State::default()), wake: Condvar::new(), idle: Condvar::new(), max_size }
    }

    /// Queues an event, or drops it (and counts it) when the queue is full
    /// or closed.
    pub(crate) fn push(&self, event: PendingEvent, batch_size: usize) {
        let mut state = lock(&self.state);
        if state.closed || state.events.len() >= self.max_size {
            state.dropped += 1;
            let dropped = state.dropped;
            drop(state);
            log::warn_every(
                "queue_full",
                Duration::from_secs(60),
                format!("the event queue is full; {dropped} event(s) dropped so far"),
            );
            return;
        }
        state.events.push_back(event);
        if state.events.len() >= batch_size {
            self.wake.notify_all();
        }
    }

    /// Waits for a batch: `batch_size` events, or whatever is queued once
    /// `interval` has passed (or a flush or close was asked for). An empty
    /// result with the queue closed ends the worker.
    fn take_batch(&self, batch_size: usize, interval: Duration) -> (Vec<PendingEvent>, bool) {
        let deadline = Instant::now() + interval;
        let mut state = lock(&self.state);
        loop {
            let ready = state.events.len() >= batch_size || state.closed || state.flush_requested;
            let now = Instant::now();
            if ready || now >= deadline {
                break;
            }
            state = self
                .wake
                .wait_timeout(state, deadline - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        let count = state.events.len().min(batch_size);
        let batch: Vec<PendingEvent> = state.events.drain(..count).collect();
        state.in_flight += batch.len();
        if state.events.is_empty() {
            state.flush_requested = false;
        }
        let closed = state.closed;
        (batch, closed)
    }

    fn batch_done(&self, count: usize) {
        let mut state = lock(&self.state);
        state.in_flight = state.in_flight.saturating_sub(count);
        if state.events.is_empty() && state.in_flight == 0 {
            self.idle.notify_all();
        }
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }

    /// Sleeps for `duration`, returning early (false) when the queue closes.
    fn pause(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        let mut state = lock(&self.state);
        while !state.closed {
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            state = self
                .wake
                .wait_timeout(state, deadline - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        false
    }

    /// Waits until everything queued has been handled, up to `timeout`.
    /// Returns true when the queue drained in time.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.state);
        state.flush_requested = true;
        self.wake.notify_all();
        while !(state.events.is_empty() && state.in_flight == 0) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .idle
                .wait_timeout(state, deadline - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        true
    }

    /// Stops taking events and lets the worker drain what is queued.
    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        self.wake.notify_all();
    }
}

/// The worker thread and its queue.
pub(crate) struct Worker {
    pub(crate) queue: Arc<Queue>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

/// What the worker needs besides the queue.
pub(crate) struct Sender {
    pub(crate) transport: Arc<Transport>,
    pub(crate) paths: Arc<Paths>,
    pub(crate) source: Option<SourceCache>,
    pub(crate) before_send: Option<Arc<dyn Fn(Value) -> Option<Value> + Send + Sync>>,
    pub(crate) batch_size: usize,
    pub(crate) flush_interval: Duration,
    pub(crate) max_retries: u32,
    pub(crate) batch_supported: bool,
}

impl Worker {
    pub(crate) fn start(config: &Config, transport: Arc<Transport>, paths: Arc<Paths>) -> Option<Worker> {
        let queue = Arc::new(Queue::new(config.max_queue_size));
        let sender = Sender {
            transport,
            paths,
            source: config.send_code_context.then(SourceCache::new),
            before_send: config.before_send.clone(),
            batch_size: config.batch_size,
            flush_interval: config.flush_interval,
            max_retries: config.max_retries,
            batch_supported: true,
        };
        let thread_queue = queue.clone();
        let handle = std::thread::Builder::new()
            .name("goodsorcery-worker".into())
            .spawn(move || {
                crate::client::mark_sdk_thread();
                sender.run(&thread_queue);
            })
            .map_err(|_| log::warn("could not start the worker thread; errors will not be reported"))
            .ok()?;
        Some(Worker { queue, handle: Mutex::new(Some(handle)) })
    }

    /// Closes the queue and waits up to `timeout` for it to drain.
    pub(crate) fn close(&self, timeout: Duration) -> bool {
        self.queue.close();
        let drained = self.queue.flush(timeout);
        if drained {
            if let Some(handle) = lock(&self.handle).take() {
                let _ = handle.join();
            }
        } else {
            log::warn("the event queue did not drain before the shutdown timeout; some events were not sent");
        }
        drained
    }
}

impl Sender {
    fn run(mut self, queue: &Queue) {
        loop {
            let (batch, closed) = queue.take_batch(self.batch_size, self.flush_interval);
            if batch.is_empty() {
                if closed {
                    break;
                }
                continue;
            }
            let count = batch.len();
            let sent = guard(|| {
                let jsons: Vec<Vec<u8>> = batch.into_iter().filter_map(|event| self.serialize(event)).collect();
                self.send(queue, jsons);
            });
            if sent.is_none() {
                log::warn(format!("the worker hit an internal error; {count} event(s) dropped"));
            }
            queue.batch_done(count);
        }
    }

    /// Resolves the backtrace, runs `before_send` and serializes, keeping
    /// the event under the server's 512 KiB limit.
    pub(crate) fn serialize(&self, event: PendingEvent) -> Option<Vec<u8>> {
        let PendingEvent { mut body, backtrace, site } = event;
        let mut frames = match &backtrace {
            Some(trace) => guard(|| backtrace::frames(trace, &self.paths, self.source.as_ref())).unwrap_or_default(),
            None => Vec::new(),
        };
        if frames.is_empty() {
            if let Some(site) = site {
                frames.push(Value::Object(backtrace::frame_json(
                    &backtrace::RawFrame {
                        symbol: None,
                        file: Some(site.file),
                        line: Some(site.line),
                        column: Some(site.column),
                    },
                    &self.paths,
                )));
            }
        }
        if !frames.is_empty() {
            if let Some(Value::Object(exception)) = body.get_mut("exception") {
                exception.insert("backtrace".into(), Value::Array(frames));
            }
        }
        let mut value = Value::Object(body);
        if let Some(callback) = &self.before_send {
            value = guard(|| callback(value))??;
        }
        let mut json = serde_json::to_vec(&value).ok()?;
        if json.len() > MAX_EVENT_BYTES {
            shrink(&mut value);
            json = serde_json::to_vec(&value).ok()?;
            if json.len() > MAX_EVENT_BYTES {
                log::warn_every(
                    "event_too_large",
                    Duration::from_secs(60),
                    "an event was over 512 KiB after trimming; dropped",
                );
                return None;
            }
        }
        Some(json)
    }

    fn send(&mut self, queue: &Queue, jsons: Vec<Vec<u8>>) {
        let mut pending = jsons;
        let mut attempt = 0;
        while !pending.is_empty() {
            let (outcome, unsent) = self.post_all(pending);
            pending = unsent;
            if pending.is_empty() {
                break;
            }
            match outcome {
                Outcome::RateLimited(wait) => {
                    if !queue.pause(wait) {
                        return self.give_up(&outcome, pending.len());
                    }
                }
                _ if attempt >= self.max_retries || queue.is_closed() => return self.give_up(&outcome, pending.len()),
                _ => {
                    let delay = backoff(attempt);
                    attempt += 1;
                    if !queue.pause(delay) {
                        return self.give_up(&outcome, pending.len());
                    }
                }
            }
        }
    }

    fn give_up(&self, outcome: &Outcome, count: usize) {
        log::warn_every(
            "delivery_failed",
            Duration::from_secs(60),
            format!("could not deliver {count} event(s) ({}); dropped", outcome.description()),
        );
    }

    /// Sends events in as few requests as possible. Returns the outcome
    /// that stopped it (429 or retryable) with the events still to send.
    fn post_all(&mut self, jsons: Vec<Vec<u8>>) -> (Outcome, Vec<Vec<u8>>) {
        if !self.batch_supported {
            return self.post_each(jsons);
        }
        let chunks = chunk(jsons);
        let mut remaining: VecDeque<Vec<Vec<u8>>> = chunks.into();
        while let Some(batch) = remaining.pop_front() {
            let (outcome, unsent) = self.post_batch(batch);
            match outcome {
                Outcome::NotFound => {
                    log::debug("the server has no batch endpoint; sending events one per request");
                    self.batch_supported = false;
                    let rest: Vec<Vec<u8>> = unsent.into_iter().chain(remaining.into_iter().flatten()).collect();
                    return self.post_each(rest);
                }
                Outcome::RateLimited(_) | Outcome::Retryable(_) => {
                    let rest: Vec<Vec<u8>> = unsent.into_iter().chain(remaining.into_iter().flatten()).collect();
                    return (outcome, rest);
                }
                _ => {}
            }
        }
        (Outcome::Success(Value::Null), Vec::new())
    }

    /// One batch request; a 413 splits it in halves.
    fn post_batch(&self, batch: Vec<Vec<u8>>) -> (Outcome, Vec<Vec<u8>>) {
        let outcome = self.transport.post(BATCH_PATH, &batch_body(&batch));
        match &outcome {
            Outcome::Rejected(413) if batch.len() > 1 => {
                let mut batch = batch;
                let second = batch.split_off(batch.len() / 2);
                for half in [batch, second] {
                    let (outcome, unsent) = self.post_batch(half);
                    if !unsent.is_empty() {
                        return (outcome, unsent);
                    }
                }
                (Outcome::Success(Value::Null), Vec::new())
            }
            Outcome::Success(body) => {
                let rejected = body.get("rejected").and_then(Value::as_array).map_or(0, Vec::len);
                if rejected > 0 {
                    log::warn_every(
                        "batch_rejections",
                        Duration::from_secs(60),
                        format!("the server rejected {rejected} event(s) of a batch as invalid"),
                    );
                }
                (outcome, Vec::new())
            }
            Outcome::NotFound | Outcome::RateLimited(_) | Outcome::Retryable(_) => (outcome, batch),
            Outcome::Rejected(_) => (outcome, Vec::new()),
        }
    }

    fn post_each(&self, jsons: Vec<Vec<u8>>) -> (Outcome, Vec<Vec<u8>>) {
        let mut jsons: VecDeque<Vec<u8>> = jsons.into();
        while let Some(json) = jsons.pop_front() {
            let outcome = self.transport.post(EVENTS_PATH, &json);
            if matches!(outcome, Outcome::RateLimited(_) | Outcome::Retryable(_)) {
                jsons.push_front(json);
                return (outcome, jsons.into());
            }
        }
        (Outcome::Success(Value::Null), Vec::new())
    }
}

/// Splits events into batches of at most 100 and about 2 MiB.
fn chunk(jsons: Vec<Vec<u8>>) -> Vec<Vec<Vec<u8>>> {
    let mut chunks = Vec::new();
    let mut current: Vec<Vec<u8>> = Vec::new();
    let mut bytes = 0;
    for json in jsons {
        if !current.is_empty() && (current.len() >= 100 || bytes + json.len() + 1 > MAX_BATCH_BYTES) {
            chunks.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes += json.len() + 1;
        current.push(json);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// `{"events":[…]}` from already serialized events.
fn batch_body(jsons: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::with_capacity(jsons.iter().map(|json| json.len() + 1).sum::<usize>() + 12);
    body.extend_from_slice(b"{\"events\":[");
    for (index, json) in jsons.iter().enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.extend_from_slice(json);
    }
    body.extend_from_slice(b"]}");
    body
}

/// About 1, 2, 4, 8 … seconds, capped at 60, times a random 0.5–1.
fn backoff(attempt: u32) -> Duration {
    let base = 2f64.powi(attempt.min(10) as i32).min(MAX_BACKOFF);
    Duration::from_secs_f64(base * (0.5 + random_unit() * 0.5))
}

/// Trims an oversized event: breadcrumbs first, then code context and
/// locals, then the free-form sections.
fn shrink(event: &mut Value) {
    let Some(map) = event.as_object_mut() else { return };
    map.remove("breadcrumbs");
    if let Some(frames) = map.get_mut("exception").and_then(|e| e.get_mut("backtrace")).and_then(Value::as_array_mut) {
        for frame in frames.iter_mut() {
            if let Some(frame) = frame.as_object_mut() {
                frame.remove("context");
                frame.remove("locals");
            }
        }
    }
    if serde_json::to_vec(map).map_or(0, |json| json.len()) > MAX_EVENT_BYTES {
        for key in ["context", "request", "job"] {
            map.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_by_count_and_size() {
        let small: Vec<Vec<u8>> = (0..250).map(|_| b"{}".to_vec()).collect();
        let sizes: Vec<usize> = chunk(small).iter().map(Vec::len).collect();
        assert_eq!(sizes, [100, 100, 50]);
        let big: Vec<Vec<u8>> = (0..5).map(|_| vec![b'x'; 600 * 1024]).collect();
        let sizes: Vec<usize> = chunk(big).iter().map(Vec::len).collect();
        assert_eq!(sizes, [3, 2]);
    }

    #[test]
    fn builds_batch_bodies() {
        let body = batch_body(&[b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]);
        assert_eq!(body, b"{\"events\":[{\"a\":1},{\"b\":2}]}");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        for _ in 0..100 {
            let first = backoff(0).as_secs_f64();
            assert!((0.5..=1.0).contains(&first));
            let late = backoff(20).as_secs_f64();
            assert!((30.0..=60.0).contains(&late));
        }
    }

    #[test]
    fn shrinks_oversized_events() {
        let mut event = serde_json::json!({
            "breadcrumbs": [1, 2],
            "exception": { "class": "X", "backtrace": [{ "file": "a", "context": { "lines": ["x"] } }] },
            "context": { "big": "x".repeat(MAX_EVENT_BYTES) }
        });
        shrink(&mut event);
        assert!(event.get("breadcrumbs").is_none());
        assert!(event["exception"]["backtrace"][0].get("context").is_none());
        assert!(event.get("context").is_none());
        assert_eq!(event["exception"]["class"], "X");
    }

    #[test]
    fn a_full_queue_drops_events() {
        let queue = Queue::new(2);
        for _ in 0..3 {
            queue.push(PendingEvent { body: Default::default(), backtrace: None, site: None }, 25);
        }
        let state = lock(&queue.state);
        assert_eq!(state.events.len(), 2);
        assert_eq!(state.dropped, 1);
    }
}

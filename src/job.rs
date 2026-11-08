//! Background jobs: [`job`] runs a future as one job run, in its own scope,
//! and records it for performance monitoring.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::SystemTime;

use pin_project_lite::pin_project;
use serde_json::{Map, Value};

use crate::scope::{self, Hub};
use crate::util::{iso8601_millis, lock, truncate};

/// What a job's output says about the run: an `Err` marks it failed (in
/// performance metrics). Report the error itself with
/// [`capture_error`](crate::capture_error) if you want an issue for it; a
/// panic is reported by the panic hook.
pub trait JobOutcome {
    /// Whether the run failed.
    fn is_failure(&self) -> bool;
}

impl JobOutcome for () {
    fn is_failure(&self) -> bool {
        false
    }
}

impl<T, E> JobOutcome for Result<T, E> {
    fn is_failure(&self) -> bool {
        self.is_err()
    }
}

/// Runs `future` as one run of the background job `name`, from `queue` if
/// it has one.
///
/// Errors and panics inside it are reported with the job's name as the
/// transaction and a `job` section. With performance monitoring on, the run
/// is recorded as a `job` transaction (its database and HTTP time, and its
/// queue wait when you give [`enqueued_at`](Job::enqueued_at)).
///
/// ```no_run
/// # async fn send_receipt(order_id: u64) -> Result<(), std::io::Error> { Ok(()) }
/// # async fn run(order_id: u64, enqueued_at: std::time::SystemTime) {
/// goodsorcery::job("SendReceipt", Some("mailers"), send_receipt(order_id))
///     .adapter("apalis")
///     .enqueued_at(enqueued_at)
///     .await
///     .ok();
/// # }
/// ```
pub fn job<F>(name: impl Into<String>, queue: Option<&str>, future: F) -> Job<F>
where
    F: Future,
    F::Output: JobOutcome,
{
    Job {
        inner: future,
        state: JobState {
            name: truncate(&name.into(), 255),
            queue: queue.map(|queue| truncate(queue, 128)),
            adapter: None,
            enqueued_at: None,
            id: None,
            executions: None,
            hub: None,
            finished: false,
        },
    }
}

struct JobState {
    name: String,
    queue: Option<String>,
    adapter: Option<String>,
    enqueued_at: Option<SystemTime>,
    id: Option<String>,
    executions: Option<u32>,
    hub: Option<Hub>,
    finished: bool,
}

pin_project! {
    /// A job run; see [`job`].
    #[must_use = "futures do nothing unless polled"]
    pub struct Job<F> {
        #[pin]
        inner: F,
        state: JobState,
    }
}

impl<F> Job<F> {
    /// The job system's short name (`apalis`, `faktory`, …), shown as the
    /// job's framework and adapter.
    pub fn adapter(mut self, adapter: impl Into<String>) -> Self {
        self.state.adapter = Some(truncate(&adapter.into(), 32));
        self
    }

    /// When the job was enqueued (or scheduled for): its queue wait is the
    /// time from then until the run starts.
    pub fn enqueued_at(mut self, time: SystemTime) -> Self {
        self.state.enqueued_at = Some(time);
        self
    }

    /// The job's id in its job system.
    pub fn id(mut self, id: impl ToString) -> Self {
        self.state.id = Some(truncate(&id.to_string(), 255));
        self
    }

    /// Which attempt this is (1 for the first run).
    pub fn executions(mut self, executions: u32) -> Self {
        self.state.executions = Some(executions);
        self
    }
}

impl JobState {
    fn section(&self) -> Value {
        let mut job = Map::new();
        if let Some(adapter) = &self.adapter {
            job.insert("framework".into(), Value::String(adapter.clone()));
        }
        job.insert("class".into(), Value::String(self.name.clone()));
        if let Some(id) = &self.id {
            job.insert("id".into(), Value::String(id.clone()));
        }
        if let Some(queue) = &self.queue {
            job.insert("queue".into(), Value::String(queue.clone()));
        }
        if let Some(executions) = self.executions {
            job.insert("executions".into(), Value::from(executions));
        }
        if let Some(time) = self.enqueued_at {
            job.insert("enqueued_at".into(), Value::String(iso8601_millis(time)));
        }
        Value::Object(job)
    }

    /// The scope (and recorder) for this run, made on the first poll.
    fn start(&mut self) -> Hub {
        let mut hub = scope::unit_of_work_hub();
        // A job run inline inside a request or another job shares its
        // record of reported errors.
        if hub.reported.is_none() {
            hub.reported = Some(Default::default());
        }
        if let Some(scope) = &hub.scope {
            let mut data = lock(scope);
            data.transaction = Some(self.name.clone());
            data.job = Some(self.section());
        }
        #[cfg(feature = "performance")]
        if hub.recorder.is_none() {
            let wait = self.enqueued_at.and_then(|time| SystemTime::now().duration_since(time).ok());
            hub.recorder = crate::performance::start_job(
                &self.name,
                self.adapter.as_deref(),
                self.queue.as_deref(),
                wait,
                self.executions,
            );
        } else {
            // A job run inline inside a request or another job is part of
            // that unit of work, not a transaction of its own.
            self.finished = true;
        }
        hub
    }

    #[cfg_attr(not(feature = "performance"), allow(unused_variables))]
    fn finish(&mut self, hub: &Hub, failed: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        #[cfg(feature = "performance")]
        if let Some(recorder) = &hub.recorder {
            crate::performance::finish(recorder, None, failed);
        }
    }
}

impl<F> Future for Job<F>
where
    F: Future,
    F::Output: JobOutcome,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.project();
        let state = this.state;
        let inner = this.inner;
        if !crate::client::is_enabled() && state.hub.is_none() {
            return inner.poll(cx);
        }
        let hub = match &state.hub {
            Some(hub) => hub.clone(),
            None => {
                let hub = state.start();
                state.hub = Some(hub.clone());
                hub
            }
        };
        match scope::with_hub(hub.clone(), || catch_unwind(AssertUnwindSafe(|| inner.poll(cx)))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => {
                state.finish(&hub, output.is_failure());
                Poll::Ready(output)
            }
            Err(panic) => {
                state.finish(&hub, true);
                resume_unwind(panic)
            }
        }
    }
}

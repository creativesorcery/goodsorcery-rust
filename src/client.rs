//! The process-wide client: [`init`], the guard that flushes on drop,
//! [`flush`] and [`verify`].

use std::backtrace::Backtrace;
use std::cell::Cell;
use std::fmt;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::backtrace::Paths;
use crate::config::Config;
use crate::event::PendingEvent;
use crate::filter::KeyFilter;
use crate::runtime::RuntimeInfo;
use crate::transport::{Outcome, Transport, VERIFY_PATH};
use crate::util::{deadline_after, lock, log, masked_key};
use crate::worker::Worker;

pub(crate) struct Client {
    pub(crate) config: Config,
    pub(crate) enabled: bool,
    pub(crate) filter: KeyFilter,
    pub(crate) runtime: RuntimeInfo,
    pub(crate) transport: Option<Arc<Transport>>,
    /// How frame paths are classified and shown.
    pub(crate) paths: Option<Arc<Paths>>,
    worker: Option<Worker>,
    #[cfg(feature = "performance")]
    pub(crate) apm: Option<crate::performance::Apm>,
}

static CLIENT: RwLock<Option<Arc<Client>>> = RwLock::new(None);

/// Mirrors whether the installed client is enabled, for hot paths (every
/// tracing event) that should not take the lock.
static ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static SDK_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as one of the client's own, whose tracing
/// events and panics are never reported.
pub(crate) fn mark_sdk_thread() {
    let _ = SDK_THREAD.try_with(|flag| flag.set(true));
}

pub(crate) fn on_sdk_thread() -> bool {
    SDK_THREAD.try_with(Cell::get).unwrap_or(false)
}

/// The installed client, if any.
pub(crate) fn current() -> Option<Arc<Client>> {
    CLIENT.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// The installed client, if it is enabled.
pub(crate) fn current_enabled() -> Option<Arc<Client>> {
    current().filter(|client| client.enabled)
}

/// Whether an enabled client is installed, without taking a lock.
pub(crate) fn enabled_fast() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Whether a client is installed and reporting.
pub fn is_enabled() -> bool {
    current_enabled().is_some()
}

/// Starts the client: reads the configuration, starts the background
/// sender and installs the panic hook. Call it once, first thing in `main`,
/// and keep the guard until `main` returns: dropping it sends anything
/// still queued (waiting up to the shutdown timeout).
///
/// ```no_run
/// let _guard = goodsorcery::init(goodsorcery::Config::from_env());
/// ```
///
/// Without an API key (or in a disabled environment) the client stays off:
/// no thread starts and every call returns at once.
#[track_caller]
#[inline(never)]
pub fn init(config: Config) -> ClientGuard {
    let location = Location::caller();
    log::set_debug(config.debug);
    let client = match config.gate() {
        Ok(()) => Client::start(config, location),
        Err(reason) => {
            if reason == crate::config::BAD_ENDPOINT {
                log::warn_once("bad_endpoint", format!("the client is off: {reason}"));
            } else {
                log::debug(format!("the client is off: {reason}"));
            }
            Client::disabled(config)
        }
    };
    let client = Arc::new(client);
    {
        let root = crate::scope::root_scope();
        let mut root = lock(&root);
        root.max_breadcrumbs = Some(client.config.max_breadcrumbs);
    }
    *CLIENT.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(client.clone());
    ENABLED.store(client.enabled, Ordering::Relaxed);
    if client.enabled && client.config.install_panic_hook {
        crate::panic::install();
    }
    ClientGuard { client: Some(client) }
}

impl Client {
    fn start(config: Config, location: &'static Location<'static>) -> Client {
        let api_key = config.api_key.clone().unwrap_or_default();
        // `gate` checked it.
        let endpoint = config.valid_endpoint().unwrap_or_default();
        let transport = Arc::new(Transport::new(&endpoint, &api_key, config.connect_timeout, config.timeout));
        let paths = Arc::new(Paths::new(
            config.app_root.as_ref().and_then(|root| root.to_str().map(str::to_string)),
            config.in_app_include.clone(),
            config.in_app_exclude.clone(),
            Some(location),
            Some(Backtrace::force_capture()),
        ));
        let worker = Worker::start(&config, transport.clone(), paths.clone());
        let runtime = RuntimeInfo::new(&config);
        #[cfg(feature = "performance")]
        let apm = crate::performance::Apm::start(&config, transport.clone(), paths.clone(), runtime.clone());
        #[cfg(not(feature = "performance"))]
        if config.performance_monitoring {
            log::warn_once(
                "no_performance_feature",
                "performance monitoring needs the `performance` feature; it is off",
            );
        }
        log::debug(format!("reporting to {endpoint} with key {}", masked_key(&api_key)));
        Client {
            filter: KeyFilter::new(&config.filter_names, &config.filter_parts),
            enabled: worker.is_some(),
            runtime,
            transport: Some(transport),
            paths: Some(paths),
            worker,
            #[cfg(feature = "performance")]
            apm,
            config,
        }
    }

    fn disabled(config: Config) -> Client {
        Client {
            filter: KeyFilter::new(&config.filter_names, &config.filter_parts),
            enabled: false,
            runtime: RuntimeInfo::new(&config),
            transport: None,
            paths: None,
            worker: None,
            #[cfg(feature = "performance")]
            apm: None,
            config,
        }
    }

    pub(crate) fn enqueue(&self, event: PendingEvent) {
        if let Some(worker) = &self.worker {
            worker.queue.push(event, self.config.batch_size);
        }
    }

    /// Sends errors and performance data, waiting up to `timeout` in all
    /// (each step gets only the time the previous one left).
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let deadline = deadline_after(timeout);
        let errors = self.worker.as_ref().is_none_or(|worker| worker.queue.flush(time_left(deadline)));
        #[cfg(feature = "performance")]
        let errors = self.apm.as_ref().is_none_or(|apm| apm.flush(time_left(deadline))) && errors;
        errors
    }

    /// Sends what is left and stops, waiting up to `timeout` in all.
    fn close(&self, timeout: Duration) {
        let deadline = deadline_after(timeout);
        #[cfg(feature = "performance")]
        if let Some(apm) = &self.apm {
            apm.close(time_left(deadline));
        }
        if let Some(worker) = &self.worker {
            worker.close(time_left(deadline));
        }
    }
}

/// Keeps the client running. Dropping it sends what is still queued
/// (errors, and with performance monitoring the open minute's metrics and
/// traces), waiting up to [`Config::shutdown_timeout`], then turns the
/// client off.
#[must_use = "dropping the guard shuts the client down; keep it until main returns"]
pub struct ClientGuard {
    client: Option<Arc<Client>>,
}

impl ClientGuard {
    /// Whether this client is reporting.
    pub fn is_enabled(&self) -> bool {
        self.client.as_ref().is_some_and(|client| client.enabled)
    }

    /// Sends everything queued, waiting up to `timeout`. Returns true when
    /// everything was handled in time.
    pub fn flush(&self, timeout: Duration) -> bool {
        self.client.as_ref().is_none_or(|client| client.flush(timeout))
    }
}

impl fmt::Debug for ClientGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientGuard").field("enabled", &self.is_enabled()).finish()
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else { return };
        {
            let mut slot = CLIENT.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.as_ref().is_some_and(|installed| Arc::ptr_eq(installed, &client)) {
                *slot = None;
                ENABLED.store(false, Ordering::Relaxed);
            }
        }
        client.close(client.config.shutdown_timeout);
    }
}

/// Sends everything queued, waiting up to `timeout`. Returns true when
/// everything was handled in time (or the client is off).
pub fn flush(timeout: Duration) -> bool {
    current().is_none_or(|client| client.flush(timeout))
}

/// The result of [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// The key is valid; the environment it belongs to.
    Ok {
        /// The key's environment, such as `production`.
        environment: String,
    },
    /// The client is off (no API key, or a disabled environment).
    Disabled,
    /// The server refused the key (HTTP 401).
    InvalidKey,
    /// Something else went wrong (status or error kind, never the key).
    Failed(String),
}

/// Checks that the endpoint is reachable and the API key valid
/// (`GET /api/v1/verify`), without sending an event. It blocks while it
/// talks to the server; use it in install checks and smoke tests.
pub fn verify() -> Verification {
    let Some(client) = current_enabled() else { return Verification::Disabled };
    let Some(transport) = &client.transport else { return Verification::Disabled };
    match transport.get(VERIFY_PATH) {
        Outcome::Success(body) => Verification::Ok {
            environment: body.get("environment").and_then(|value| value.as_str()).unwrap_or("").to_string(),
        },
        Outcome::Rejected(401) => Verification::InvalidKey,
        other => Verification::Failed(other.description()),
    }
}

/// What is left until `deadline` (zero once it has passed).
fn time_left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

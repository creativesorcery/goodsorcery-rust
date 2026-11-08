//! Scopes: the user, tags, context, transaction, fingerprint and
//! breadcrumbs that go with the errors of one unit of work.
//!
//! Each thread sees the scope bound to it, else the process-wide root
//! scope. The axum layer and [`job`](crate::job) bind a fresh scope to each
//! request or job run; [`scoped`] and [`bind_current_scope`] do it for any
//! future. A future carries its scope across threads: the scope is bound
//! while the future is polled, whichever thread polls it, so no async
//! runtime is assumed.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, TryLockError};
use std::task::{Context, Poll};
use std::time::SystemTime;

use pin_project_lite::pin_project;
use serde_json::{Map, Value};

use crate::event::{EventId, Level};
use crate::util::{iso8601_millis, lock, truncate};

/// The person affected by an error. Only these four fields are sent; set the
/// ones you want to see.
///
/// ```
/// goodsorcery::set_user(Some(goodsorcery::User::new().id(5521).email("mara@example.com")));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct User {
    pub(crate) id: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) name: Option<String>,
}

impl User {
    /// A user with no fields set.
    pub fn new() -> Self {
        User::default()
    }

    /// The app's id for the user.
    pub fn id(mut self, id: impl ToString) -> Self {
        self.id = Some(truncate(&id.to_string(), 255));
        self
    }

    /// The user's email address.
    pub fn email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(truncate(&email.into(), 255));
        self
    }

    /// The user's username.
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(truncate(&username.into(), 255));
        self
    }

    /// The user's display name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(truncate(&name.into(), 255));
        self
    }

    pub(crate) fn to_json(&self) -> Option<Value> {
        let mut map = Map::new();
        for (key, value) in
            [("id", &self.id), ("email", &self.email), ("username", &self.username), ("name", &self.name)]
        {
            if let Some(value) = value {
                map.insert(key.to_string(), Value::String(value.clone()));
            }
        }
        if map.is_empty() { None } else { Some(Value::Object(map)) }
    }
}

/// Something that happened before an error, shown on its issue page.
///
/// ```
/// use goodsorcery::{Breadcrumb, Level};
/// goodsorcery::add_breadcrumb(
///     Breadcrumb::new("custom", "Cart recalculated").level(Level::Info).data("items", 3),
/// );
/// ```
#[derive(Debug, Clone)]
pub struct Breadcrumb {
    pub(crate) timestamp: SystemTime,
    pub(crate) category: String,
    pub(crate) level: Level,
    pub(crate) message: String,
    pub(crate) data: Map<String, Value>,
}

impl Breadcrumb {
    /// A breadcrumb in `category` (`request`, `controller`, `sql`, `http`,
    /// `cache`, `log`, `job` or `custom`; others become `custom` on the
    /// server).
    pub fn new(category: impl Into<String>, message: impl Into<String>) -> Self {
        Breadcrumb {
            timestamp: SystemTime::now(),
            category: category.into(),
            level: Level::Info,
            message: truncate(&message.into(), 1_000),
            data: Map::new(),
        }
    }

    /// The severity (default [`Level::Info`]).
    pub fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Adds a data field (at most 20 are kept; filtered like context).
    pub fn data(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        if self.data.len() < 20 {
            self.data.insert(key.into(), value.into());
        }
        self
    }

    pub(crate) fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("timestamp".into(), Value::String(iso8601_millis(self.timestamp)));
        map.insert("category".into(), Value::String(self.category.clone()));
        map.insert("level".into(), Value::String(self.level.breadcrumb_name().into()));
        map.insert("message".into(), Value::String(self.message.clone()));
        if !self.data.is_empty() {
            map.insert("data".into(), Value::Object(self.data.clone()));
        }
        Value::Object(map)
    }
}

/// What a scope holds.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScopeData {
    pub(crate) user: Option<User>,
    pub(crate) tags: BTreeMap<String, String>,
    pub(crate) context: Map<String, Value>,
    pub(crate) transaction: Option<String>,
    pub(crate) fingerprint: Option<Vec<String>>,
    pub(crate) breadcrumbs: VecDeque<Breadcrumb>,
    pub(crate) max_breadcrumbs: Option<usize>,
    /// The request section (already filtered), set by the axum layer.
    pub(crate) request: Option<Value>,
    /// The job section, set by [`job`](crate::job).
    pub(crate) job: Option<Value>,
}

impl ScopeData {
    /// A child: the same user, tags, context, transaction and fingerprint,
    /// with or without the breadcrumbs, and no request or job.
    pub(crate) fn fork(&self, breadcrumbs: bool) -> ScopeData {
        ScopeData {
            user: self.user.clone(),
            tags: self.tags.clone(),
            context: self.context.clone(),
            transaction: self.transaction.clone(),
            fingerprint: self.fingerprint.clone(),
            breadcrumbs: if breadcrumbs { self.breadcrumbs.clone() } else { VecDeque::new() },
            max_breadcrumbs: self.max_breadcrumbs,
            request: None,
            job: None,
        }
    }

    pub(crate) fn push_breadcrumb(&mut self, breadcrumb: Breadcrumb) {
        let max = self
            .max_breadcrumbs
            .or_else(|| crate::client::current().map(|client| client.config.max_breadcrumbs))
            .unwrap_or(40);
        if max == 0 {
            return;
        }
        while self.breadcrumbs.len() >= max {
            self.breadcrumbs.pop_front();
        }
        self.breadcrumbs.push_back(breadcrumb);
    }
}

/// The current scope, as seen by [`configure_scope`].
pub struct Scope<'a> {
    data: &'a mut ScopeData,
}

impl Scope<'_> {
    /// Sets (or clears) the affected user.
    pub fn set_user(&mut self, user: Option<User>) {
        self.data.user = user;
    }

    /// Sets a tag. Keys are 1–32 characters of letters, digits, `_`, `.`
    /// and `-`; others are dropped when sending.
    pub fn set_tag(&mut self, key: impl Into<String>, value: impl ToString) {
        self.data.tags.insert(key.into(), value.to_string());
    }

    /// Removes a tag.
    pub fn remove_tag(&mut self, key: &str) {
        self.data.tags.remove(key);
    }

    /// Adds a context section shown on the Environment tab (filtered by
    /// key).
    pub fn set_context(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.data.context.insert(key.into(), value.into());
    }

    /// Names what is running (`POST /checkout`, `nightly-report`).
    pub fn set_transaction(&mut self, name: impl Into<String>) {
        self.data.transaction = Some(name.into());
    }

    /// Groups this scope's errors by these strings instead of the default
    /// grouping.
    pub fn set_fingerprint<I, S>(&mut self, fingerprint: Option<I>)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.data.fingerprint = fingerprint.map(|parts| parts.into_iter().map(Into::into).collect());
    }

    /// Adds a breadcrumb (the oldest is dropped past the limit).
    pub fn add_breadcrumb(&mut self, breadcrumb: Breadcrumb) {
        self.data.push_breadcrumb(breadcrumb);
    }

    /// Removes every breadcrumb.
    pub fn clear_breadcrumbs(&mut self) {
        self.data.breadcrumbs.clear();
    }
}

/// What is bound to a thread while it runs a unit of work: the scope,
/// the unit of work's first reported error and, with performance
/// monitoring, the request's recorder.
#[derive(Clone, Default)]
pub(crate) struct Hub {
    pub(crate) scope: Option<Arc<Mutex<ScopeData>>>,
    /// The first error reported in this request or job run. Shared by
    /// every scope forked inside it (`with_scope`, `scoped`, an inline
    /// job), so an error captured in a child still counts for the request.
    pub(crate) reported: Option<Arc<OnceLock<EventId>>>,
    #[cfg(feature = "performance")]
    pub(crate) recorder: Option<Arc<crate::performance::recorder::Recorder>>,
}

impl Hub {
    #[cfg(test)]
    pub(crate) fn with_scope(scope: ScopeData) -> Hub {
        Hub {
            scope: Some(Arc::new(Mutex::new(scope))),
            reported: None,
            #[cfg(feature = "performance")]
            recorder: None,
        }
    }

    /// Whether an error was reported in this unit of work.
    pub(crate) fn has_reported(&self) -> bool {
        self.reported.as_ref().is_some_and(|reported| reported.get().is_some())
    }

    /// Notes the first error reported in this unit of work.
    pub(crate) fn note_reported(&self, id: EventId) {
        if let Some(reported) = &self.reported {
            let _ = reported.set(id);
        }
    }

    pub(crate) fn scope(&self) -> Arc<Mutex<ScopeData>> {
        self.scope.clone().unwrap_or_else(|| ROOT.clone())
    }
}

static ROOT: LazyLock<Arc<Mutex<ScopeData>>> = LazyLock::new(|| Arc::new(Mutex::new(ScopeData::default())));

thread_local! {
    static CURRENT: RefCell<Option<Hub>> = const { RefCell::new(None) };
    /// Set while this thread runs a `configure_scope` closure.
    static HOLDING: Cell<bool> = const { Cell::new(false) };
}

/// The hub bound to this thread, if any.
pub(crate) fn current_hub() -> Option<Hub> {
    CURRENT.try_with(|current| current.borrow().clone()).ok().flatten()
}

/// The scope errors on this thread go to.
pub(crate) fn current_scope() -> Arc<Mutex<ScopeData>> {
    current_hub().map(|hub| hub.scope()).unwrap_or_else(|| ROOT.clone())
}

/// The process-wide root scope.
pub(crate) fn root_scope() -> Arc<Mutex<ScopeData>> {
    ROOT.clone()
}

/// Runs `f` with `hub` bound to this thread, restoring the previous binding
/// afterwards (also when `f` panics).
pub(crate) fn with_hub<R>(hub: Hub, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Hub>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            let _ = CURRENT.try_with(|current| *current.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.try_with(|current| current.borrow_mut().replace(hub)).ok().flatten();
    let _restore = Restore(previous);
    f()
}

/// A child of the current hub with a fresh scope (no breadcrumbs unless
/// asked for), keeping its recorder.
pub(crate) fn forked_hub(breadcrumbs: bool) -> Hub {
    let current = current_hub().unwrap_or_default();
    let data = lock(&current.scope()).fork(breadcrumbs);
    let mut hub = current;
    hub.scope = Some(Arc::new(Mutex::new(data)));
    hub
}

/// Changes the current scope: the one bound to this request, job or
/// future, else the process-wide root scope.
///
/// ```
/// goodsorcery::configure_scope(|scope| {
///     scope.set_tag("tenant", "acme");
///     scope.set_context("cart", serde_json::json!({ "items": 3 }));
/// });
/// ```
pub fn configure_scope<R>(f: impl FnOnce(&mut Scope<'_>) -> R) -> R {
    struct Release(bool);
    impl Drop for Release {
        fn drop(&mut self) {
            let previous = self.0;
            let _ = HOLDING.try_with(|holding| holding.set(previous));
        }
    }
    let scope = current_scope();
    match lock_scope(&scope) {
        Some(mut data) => {
            let _release = Release(HOLDING.try_with(|holding| holding.replace(true)).unwrap_or(false));
            f(&mut Scope { data: &mut data })
        }
        // Called from inside another `configure_scope` on the same scope:
        // the changes go nowhere rather than deadlocking.
        None => f(&mut Scope { data: &mut ScopeData::default() }),
    }
}

/// Locks a scope, unless this thread may already hold it (inside a
/// `configure_scope` closure, or in the panic hook after a panic there):
/// then it only tries, and `None` means the scope is busy. Waiting there
/// would deadlock.
pub(crate) fn lock_scope(scope: &Mutex<ScopeData>) -> Option<MutexGuard<'_, ScopeData>> {
    let careful = HOLDING.try_with(Cell::get).unwrap_or(true) || crate::panic::in_hook();
    if !careful {
        return Some(lock(scope));
    }
    match scope.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Runs `f` in a child of the current scope: changes made inside stay
/// inside.
pub fn with_scope<R>(f: impl FnOnce() -> R) -> R {
    with_hub(forked_hub(true), f)
}

/// Sets (or clears) the affected user in the current scope.
pub fn set_user(user: Option<User>) {
    configure_scope(|scope| scope.set_user(user));
}

/// Sets a tag in the current scope.
pub fn set_tag(key: impl Into<String>, value: impl ToString) {
    configure_scope(|scope| scope.set_tag(key, value));
}

/// Adds a context section to the current scope.
pub fn set_context(key: impl Into<String>, value: impl Into<Value>) {
    configure_scope(|scope| scope.set_context(key, value));
}

/// Names what is running in the current scope.
pub fn set_transaction(name: impl Into<String>) {
    configure_scope(|scope| scope.set_transaction(name));
}

/// Adds a breadcrumb to the current scope.
pub fn add_breadcrumb(breadcrumb: Breadcrumb) {
    if crate::client::is_enabled() {
        configure_scope(|scope| scope.add_breadcrumb(breadcrumb));
    }
}

pin_project! {
    /// A future running with its own scope bound while it is polled. See
    /// [`scoped`] and [`bind_current_scope`].
    #[must_use = "futures do nothing unless polled"]
    pub struct Scoped<F> {
        hub: Hub,
        #[pin]
        inner: F,
    }
}

impl<F> Scoped<F> {
    pub(crate) fn new(hub: Hub, inner: F) -> Self {
        Scoped { hub, inner }
    }
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.project();
        let inner = this.inner;
        with_hub(this.hub.clone(), || inner.poll(cx))
    }
}

/// Runs `future` in a fresh child of the current scope (breadcrumbs
/// included), for work that should keep its own user, tags or breadcrumbs.
pub fn scoped<F: Future>(future: F) -> Scoped<F> {
    Scoped::new(forked_hub(true), future)
}

/// Runs `future` in the current scope, so that a task spawned from a request
/// handler still reports errors with that request's data:
///
/// ```no_run
/// # async fn send_receipt() {}
/// # async fn handler() {
/// tokio::spawn(goodsorcery::bind_current_scope(async move {
///     send_receipt().await;
/// }));
/// # }
/// ```
pub fn bind_current_scope<F: Future>(future: F) -> Scoped<F> {
    Scoped::new(current_hub().unwrap_or_default(), future)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_scope_keeps_changes_inside() {
        with_hub(Hub::with_scope(ScopeData::default()), || {
            set_tag("outer", 1);
            with_scope(|| {
                set_tag("inner", 2);
                assert_eq!(lock(&current_scope()).tags.len(), 2);
            });
            let scope = current_scope();
            let data = lock(&scope);
            assert!(data.tags.contains_key("outer"));
            assert!(!data.tags.contains_key("inner"));
        });
    }

    #[test]
    fn breadcrumbs_are_bounded() {
        let mut data = ScopeData { max_breadcrumbs: Some(3), ..ScopeData::default() };
        for n in 0..5 {
            data.push_breadcrumb(Breadcrumb::new("custom", format!("crumb {n}")));
        }
        let messages: Vec<_> = data.breadcrumbs.iter().map(|b| b.message.clone()).collect();
        assert_eq!(messages, ["crumb 2", "crumb 3", "crumb 4"]);
    }

    #[test]
    fn user_json_has_only_set_fields() {
        let json = User::new().id(5521).username("mara").to_json().unwrap();
        assert_eq!(json, serde_json::json!({ "id": "5521", "username": "mara" }));
        assert!(User::new().to_json().is_none());
    }

    #[test]
    fn breadcrumb_data_is_capped() {
        let mut crumb = Breadcrumb::new("custom", "x".repeat(2_000)).level(Level::Warning);
        for n in 0..30 {
            crumb = crumb.data(format!("k{n}"), n);
        }
        let json = crumb.to_json();
        assert_eq!(json["data"].as_object().unwrap().len(), 20);
        assert_eq!(json["message"].as_str().unwrap().len(), 1_000);
        assert_eq!(json["level"], "warning");
    }
}

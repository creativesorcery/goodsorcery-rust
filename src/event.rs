//! Capturing errors and messages, and building the event payload
//! (`docs/INGEST_API.md` › Body).

use std::backtrace::Backtrace;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::panic::Location;
use std::time::SystemTime;

use serde_json::{Map, Value};

use crate::client::{self, Client};
use crate::scope::{self, User};
use crate::util::{format_uuid, iso8601_millis, truncate, uuid_v7};

const MAX_CLASS: usize = 255;
const MAX_MESSAGE: usize = 10_000;
const MAX_CAUSE_MESSAGE: usize = 1_000;
const MAX_CAUSES: usize = 3;
const MAX_TRANSACTION: usize = 1_000;

/// The id of a reported event (a UUIDv7), as the server knows it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventId([u8; 16]);

impl EventId {
    pub(crate) fn new() -> Self {
        EventId(uuid_v7(SystemTime::now()))
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_uuid(&self.0))
    }
}

impl fmt::Debug for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EventId({self})")
    }
}

/// Severity of an event or breadcrumb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Level {
    /// Breadcrumbs only; sent as `info` for events.
    Debug,
    /// Informational.
    Info,
    /// Something unexpected that was handled.
    Warning,
    /// An error (the default for errors and panics).
    #[default]
    Error,
    /// The process cannot go on; sent as `error` for breadcrumbs.
    Fatal,
}

impl Level {
    pub(crate) fn event_name(self) -> &'static str {
        match self {
            Level::Debug | Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
            Level::Fatal => "fatal",
        }
    }

    pub(crate) fn breadcrumb_name(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error | Level::Fatal => "error",
        }
    }
}

/// Extra data for one event, passed to [`capture_error_with`] or
/// [`capture_message_with`]. It is merged over the current scope.
///
/// ```
/// use goodsorcery::{Level, Options};
/// let err = std::io::Error::other("disk full");
/// goodsorcery::capture_error_with(
///     &err,
///     Options::new().level(Level::Warning).tag("disk", "data").fingerprint(["disk-full"]),
/// );
/// ```
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub(crate) level: Option<Level>,
    pub(crate) fingerprint: Option<Vec<String>>,
    pub(crate) tags: BTreeMap<String, String>,
    pub(crate) context: Map<String, Value>,
    pub(crate) transaction: Option<String>,
    pub(crate) user: Option<User>,
}

impl Options {
    /// No options.
    pub fn new() -> Self {
        Options::default()
    }

    /// The event's severity.
    pub fn level(mut self, level: Level) -> Self {
        self.level = Some(level);
        self
    }

    /// Groups the event by these strings instead of the default grouping.
    pub fn fingerprint<I, S>(mut self, parts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.fingerprint = Some(parts.into_iter().map(Into::into).collect());
        self
    }

    /// A tag for this event.
    pub fn tag(mut self, key: impl Into<String>, value: impl ToString) -> Self {
        self.tags.insert(key.into(), value.to_string());
        self
    }

    /// A context section for this event (filtered by key).
    pub fn context(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// The transaction name for this event.
    pub fn transaction(mut self, name: impl Into<String>) -> Self {
        self.transaction = Some(name.into());
        self
    }

    /// The affected user for this event.
    pub fn user(mut self, user: User) -> Self {
        self.user = Some(user);
        self
    }
}

/// The exception section before its backtrace is resolved.
#[derive(Debug, Clone)]
pub(crate) struct ExceptionInfo {
    pub(crate) class: String,
    pub(crate) message: String,
    pub(crate) causes: Vec<(String, String)>,
}

/// A synthetic top frame for when the backtrace has no usable frames (a
/// stripped binary): the panic location or the capture call site.
#[derive(Debug, Clone)]
pub(crate) struct Site {
    pub(crate) file: String,
    pub(crate) line: u32,
    pub(crate) column: u32,
}

impl From<&Location<'_>> for Site {
    fn from(location: &Location<'_>) -> Self {
        Site { file: location.file().to_string(), line: location.line(), column: location.column() }
    }
}

/// An event on its way to the sender thread: the payload without its
/// backtrace, which is resolved there.
pub(crate) struct PendingEvent {
    pub(crate) body: Map<String, Value>,
    pub(crate) backtrace: Option<Backtrace>,
    pub(crate) site: Option<Site>,
}

/// Reports an error, with its `source()` chain, the current scope and a
/// backtrace of the call site. Returns the event id, or `None` when the
/// client is off.
///
/// It never panics or blocks: the event is queued and sent in the
/// background.
///
/// ```
/// # fn charge_card() -> Result<(), std::io::Error> { Ok(()) }
/// if let Err(err) = charge_card() {
///     goodsorcery::capture_error(&err);
/// }
/// ```
#[track_caller]
#[inline(never)]
pub fn capture_error<E: Error + ?Sized>(error: &E) -> Option<EventId> {
    capture_error_with(error, Options::default())
}

/// [`capture_error`] with extra [`Options`].
#[track_caller]
#[inline(never)]
pub fn capture_error_with<E: Error + ?Sized>(error: &E, options: Options) -> Option<EventId> {
    let site = Location::caller();
    let client = client::current_enabled()?;
    let info = guard(|| exception_info(error))?;
    client.capture(info, options, Some(Backtrace::force_capture()), Some(site.into()))
}

/// Reports an [`anyhow::Error`]: its outermost message, its chain of causes
/// and a backtrace of the call site.
#[cfg(feature = "anyhow")]
#[track_caller]
#[inline(never)]
pub fn capture_anyhow(error: &anyhow::Error) -> Option<EventId> {
    let site = Location::caller();
    let client = client::current_enabled()?;
    let info = guard(|| {
        let root = error.root_cause();
        ExceptionInfo {
            class: known_class(root)
                .map(str::to_string)
                .or_else(|| debug_class(root))
                .unwrap_or_else(|| "anyhow::Error".into()),
            message: truncate(&error.to_string(), MAX_MESSAGE),
            causes: error
                .chain()
                .skip(1)
                .take(MAX_CAUSES)
                .map(|cause| (dyn_class(cause), truncate(&cause.to_string(), MAX_CAUSE_MESSAGE)))
                .collect(),
        }
    })?;
    client.capture(info, Options::default(), Some(Backtrace::force_capture()), Some(site.into()))
}

/// Reports a message (no error value) at `level`, with a backtrace of the
/// call site.
#[track_caller]
#[inline(never)]
pub fn capture_message(message: impl Into<String>, level: Level) -> Option<EventId> {
    capture_message_with(message, Options::new().level(level))
}

/// [`capture_message`] with extra [`Options`]; the level comes from the
/// options (default `info`).
#[track_caller]
#[inline(never)]
pub fn capture_message_with(message: impl Into<String>, mut options: Options) -> Option<EventId> {
    let site = Location::caller();
    let client = client::current_enabled()?;
    let message = truncate(&message.into(), MAX_MESSAGE);
    options.level = Some(options.level.unwrap_or(Level::Info));
    let info = ExceptionInfo { class: "Message".into(), message, causes: Vec::new() };
    client.capture(info, options, Some(Backtrace::force_capture()), Some(site.into()))
}

/// Runs `f`, turning a panic (from an app's `Display` or `Debug`) into
/// `None`.
pub(crate) fn guard<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

pub(crate) fn exception_info<E: Error + ?Sized>(error: &E) -> ExceptionInfo {
    let class =
        type_class(std::any::type_name::<E>()).unwrap_or_else(|| debug_class(error).unwrap_or_else(|| "Error".into()));
    let mut causes = Vec::new();
    let mut seen: Vec<*const ()> = Vec::new();
    let mut current = error.source();
    while let Some(cause) = current {
        let address = cause as *const dyn Error as *const ();
        if causes.len() >= MAX_CAUSES || seen.contains(&address) {
            break;
        }
        seen.push(address);
        causes.push((dyn_class(cause), truncate(&cause.to_string(), MAX_CAUSE_MESSAGE)));
        current = cause.source();
    }
    ExceptionInfo { class: truncate(&class, MAX_CLASS), message: truncate(&error.to_string(), MAX_MESSAGE), causes }
}

/// The class of an error known only as `dyn Error`.
fn dyn_class(error: &(dyn Error + 'static)) -> String {
    known_class(error).map(str::to_string).or_else(|| debug_class(error)).unwrap_or_else(|| "Error".into())
}

/// Standard library errors recognised by downcasting.
fn known_class(error: &(dyn Error + 'static)) -> Option<&'static str> {
    macro_rules! known {
        ($($ty:ty => $name:literal),* $(,)?) => {
            $(if error.is::<$ty>() { return Some($name); })*
        };
    }
    known! {
        std::io::Error => "std::io::Error",
        std::fmt::Error => "std::fmt::Error",
        std::num::ParseIntError => "std::num::ParseIntError",
        std::num::ParseFloatError => "std::num::ParseFloatError",
        std::num::TryFromIntError => "std::num::TryFromIntError",
        std::str::Utf8Error => "std::str::Utf8Error",
        std::string::FromUtf8Error => "std::string::FromUtf8Error",
        std::str::ParseBoolError => "std::str::ParseBoolError",
        std::time::SystemTimeError => "std::time::SystemTimeError",
        std::env::VarError => "std::env::VarError",
        std::net::AddrParseError => "std::net::AddrParseError",
        serde_json::Error => "serde_json::Error",
    }
    None
}

/// A readable class from `std::any::type_name`: `std::io::error::Error` →
/// `std::io::Error` (a private module named after its type is dropped).
/// `None` for trait objects, whose concrete type is unknown.
pub(crate) fn type_class(name: &str) -> Option<String> {
    let name = name.trim_start_matches('&');
    if name.starts_with("dyn ") || name.starts_with("(dyn ") {
        return None;
    }
    let generics = name.find('<').unwrap_or(name.len());
    let (path, rest) = name.split_at(generics);
    let mut segments: Vec<&str> = path.split("::").collect();
    if segments.len() >= 3 {
        let last = segments[segments.len() - 1];
        let module = segments[segments.len() - 2];
        if module == last.to_ascii_lowercase() {
            segments.remove(segments.len() - 2);
        }
    }
    let class = format!("{}{rest}", segments.join("::"));
    let class = match class.as_str() {
        "std::io::Error" | "core::fmt::Error" => class.replacen("core::", "std::", 1),
        _ => class,
    };
    Some(class)
}

/// The leading type or variant name of an error's `Debug` output, for
/// trait objects (`NotFound(…)` → `NotFound`).
fn debug_class(error: &(impl Error + ?Sized)) -> Option<String> {
    let text = guard(|| format!("{error:?}"))?;
    let end = text
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_' || *c == ':'))
        .map_or(text.len(), |(at, _)| at);
    let name = &text[..end];
    let rest = text[end..].trim_start();
    let first_upper = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    let shaped = rest.is_empty() || rest.starts_with('(') || rest.starts_with('{');
    (first_upper && shaped && name.len() <= MAX_CLASS).then(|| name.to_string())
}

impl Client {
    /// Builds the event (minus the backtrace) in the caller and queues it.
    pub(crate) fn capture(
        &self,
        info: ExceptionInfo,
        options: Options,
        backtrace: Option<Backtrace>,
        site: Option<Site>,
    ) -> Option<EventId> {
        let id = EventId::new();
        let body = guard(|| self.event_body(id, info, options))?;
        record_event_id(id);
        self.enqueue(PendingEvent { body, backtrace, site });
        Some(id)
    }

    fn event_body(&self, id: EventId, info: ExceptionInfo, options: Options) -> Map<String, Value> {
        let scope = scope::current_scope();
        let scope = scope::lock_scope(&scope).map(|data| data.clone()).unwrap_or_default();
        let mut event = Map::new();
        event.insert("event_id".into(), Value::String(id.to_string()));
        event.insert("occurred_at".into(), Value::String(iso8601_millis(SystemTime::now())));
        event.insert("level".into(), Value::String(options.level.unwrap_or_default().event_name().into()));
        if let Some(release) = &self.config.release {
            event.insert("release".into(), Value::String(release.clone()));
        }
        if let Some(transaction) = options.transaction.as_ref().or(scope.transaction.as_ref()) {
            event.insert("transaction".into(), Value::String(truncate(transaction, MAX_TRANSACTION)));
        }
        let mut exception = Map::new();
        exception.insert("class".into(), Value::String(info.class));
        exception.insert("message".into(), Value::String(info.message));
        event.insert("exception".into(), Value::Object(exception));
        if let Some(request) = scope.request.clone() {
            event.insert("request".into(), request);
        }
        if let Some(job) = scope.job.clone() {
            event.insert("job".into(), job);
        }
        if self.config.send_user {
            if let Some(user) = options.user.as_ref().or(scope.user.as_ref()).and_then(User::to_json) {
                event.insert("user".into(), user);
            }
        }
        let breadcrumbs: Vec<Value> =
            scope.breadcrumbs.iter().rev().take(100).rev().map(|crumb| self.breadcrumb_json(crumb)).collect();
        if !breadcrumbs.is_empty() {
            event.insert("breadcrumbs".into(), Value::Array(breadcrumbs));
        }
        event.insert("context".into(), Value::Object(self.context(&info.causes, &scope.context, options.context)));
        let tags = self.tags(&scope.tags, options.tags);
        if !tags.is_empty() {
            event.insert("tags".into(), Value::Object(tags));
        }
        if let Some(fingerprint) = options.fingerprint.or(scope.fingerprint) {
            let parts: Vec<Value> =
                fingerprint.iter().take(10).map(|part| Value::String(truncate(part, 200))).collect();
            if !parts.is_empty() {
                event.insert("fingerprint".into(), Value::Array(parts));
            }
        }
        event.insert("notifier".into(), crate::runtime::notifier());
        event
    }

    fn breadcrumb_json(&self, crumb: &crate::scope::Breadcrumb) -> Value {
        let mut json = crumb.to_json();
        if let Some(Value::Object(data)) = json.get_mut("data") {
            self.filter.filter_map(data);
            for value in data.values_mut() {
                crate::filter::bound(value);
            }
        }
        json
    }

    fn context(
        &self,
        causes: &[(String, String)],
        scope: &Map<String, Value>,
        extra: Map<String, Value>,
    ) -> Map<String, Value> {
        let mut context = self.runtime.context();
        if !causes.is_empty() {
            let list = causes
                .iter()
                .map(|(class, message)| serde_json::json!({ "class": class, "message": message }))
                .collect();
            context.insert("cause".into(), Value::Array(list));
        }
        let mut added = scope.clone();
        added.extend(extra);
        let mut added = Value::Object(added);
        self.filter.filter(&mut added);
        crate::filter::bound(&mut added);
        if let Value::Object(added) = added {
            context.extend(added);
        }
        context
    }

    /// Config tags, then scope tags, then the event's own, keeping valid
    /// keys (`[a-z0-9_.-]{1,32}`), at most 25, values ≤ 200 characters.
    fn tags(&self, scope: &BTreeMap<String, String>, extra: BTreeMap<String, String>) -> Map<String, Value> {
        let mut merged = self.config.tags.clone();
        merged.extend(scope.iter().map(|(k, v)| (k.clone(), v.clone())));
        merged.extend(extra);
        let mut tags = Map::new();
        for (key, value) in merged {
            if tags.len() >= 25 {
                break;
            }
            if valid_tag_key(&key) {
                tags.insert(key, Value::String(truncate(&value, 200)));
            }
        }
        self.filter.filter_map(&mut tags);
        tags
    }
}

pub(crate) fn valid_tag_key(key: &str) -> bool {
    (1..=32).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Notes the event in the current scope (the first one wins) and on the
/// current performance recorder.
fn record_event_id(id: EventId) {
    let scope = scope::current_scope();
    if let Some(mut data) = scope::lock_scope(&scope) {
        if data.event_id.is_none() {
            data.event_id = Some(id);
        }
    }
    #[cfg(feature = "performance")]
    crate::performance::note_error(id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct CartError {
        source: std::io::Error,
    }
    impl fmt::Display for CartError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("variant 4410 is discontinued")
        }
    }
    impl Error for CartError {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn class_message_and_causes() {
        let error = CartError { source: std::io::Error::other("disk full") };
        let info = exception_info(&error);
        assert_eq!(info.class, "goodsorcery::event::tests::CartError");
        assert_eq!(info.message, "variant 4410 is discontinued");
        assert_eq!(info.causes, vec![("std::io::Error".to_string(), "disk full".to_string())]);
    }

    #[test]
    fn std_type_names_are_tidied() {
        assert_eq!(type_class("std::io::error::Error").as_deref(), Some("std::io::Error"));
        assert_eq!(type_class("serde_json::error::Error").as_deref(), Some("serde_json::Error"));
        assert_eq!(type_class("core::num::error::ParseIntError").as_deref(), Some("core::num::error::ParseIntError"));
        assert_eq!(type_class("&shop::Error").as_deref(), Some("shop::Error"));
        assert_eq!(type_class("dyn core::error::Error + Send + Sync"), None);
        let io = std::io::Error::other("x");
        assert_eq!(exception_info(&io).class, "std::io::Error");
    }

    #[test]
    fn trait_objects_fall_back_to_the_debug_name() {
        #[derive(Debug)]
        enum ShopError {
            NotFound(#[allow(dead_code)] u32),
        }
        impl fmt::Display for ShopError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("not found")
            }
        }
        impl Error for ShopError {}
        let boxed: Box<dyn Error + Send + Sync> = Box::new(ShopError::NotFound(7));
        assert_eq!(exception_info(&*boxed).class, "NotFound");
        let message: Box<dyn Error + Send + Sync> = "plain message".into();
        assert_eq!(exception_info(&*message).class, "Error");
    }

    #[test]
    fn levels() {
        assert_eq!(Level::Debug.event_name(), "info");
        assert_eq!(Level::Fatal.breadcrumb_name(), "error");
        assert_eq!(Level::default(), Level::Error);
    }

    #[test]
    fn tag_keys() {
        assert!(valid_tag_key("region"));
        assert!(valid_tag_key("a.b-c_d"));
        assert!(!valid_tag_key(""));
        assert!(!valid_tag_key("has space"));
        assert!(!valid_tag_key(&"k".repeat(33)));
    }
}

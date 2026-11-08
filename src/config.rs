//! Configuration: [`Config::from_env`] plus builder methods.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::filter::DEFAULT_FILTER_PARTS;
use crate::util::{masked_key, truncate};

/// The hosted service. Self-hosted instances set `GOODSORCERY_ENDPOINT` or
/// [`Config::endpoint`].
pub const DEFAULT_ENDPOINT: &str = "https://app.goodsorcery.com";

/// Environment variables tried, in order, for the release.
const RELEASE_VARS: &[&str] = &[
    "GOODSORCERY_RELEASE",
    "KAMAL_VERSION",
    "HEROKU_SLUG_COMMIT",
    "HEROKU_BUILD_COMMIT",
    "RENDER_GIT_COMMIT",
    "GIT_COMMIT",
    "GIT_SHA",
    "SOURCE_VERSION",
];

type BeforeSend = Arc<dyn Fn(Value) -> Option<Value> + Send + Sync>;

/// How the client behaves. Start from [`Config::from_env`] (or
/// [`Config::new`] to ignore the environment) and chain builder methods:
///
/// ```
/// let config = goodsorcery::Config::from_env()
///     .release(env!("CARGO_PKG_VERSION"))
///     .tag("region", "eu-west-1")
///     .performance_monitoring(true);
/// # let _ = config;
/// ```
///
/// Without an API key the client stays off: nothing is captured, no thread
/// starts and every call returns at once.
#[derive(Clone)]
pub struct Config {
    pub(crate) api_key: Option<String>,
    pub(crate) endpoint: String,
    pub(crate) environment: String,
    pub(crate) enabled: Option<bool>,
    pub(crate) disabled_environments: Vec<String>,
    pub(crate) enabled_environments: Option<Vec<String>>,
    pub(crate) release: Option<String>,
    pub(crate) server_name: Option<String>,
    pub(crate) filter_names: Vec<String>,
    pub(crate) filter_parts: Vec<String>,
    pub(crate) send_request_params: bool,
    pub(crate) send_request_headers: bool,
    pub(crate) send_user: bool,
    pub(crate) request_id_header: Option<String>,
    pub(crate) send_code_context: bool,
    pub(crate) capture_server_errors: bool,
    pub(crate) max_breadcrumbs: usize,
    pub(crate) tags: BTreeMap<String, String>,
    pub(crate) runtimes: Vec<(String, String)>,
    pub(crate) before_send: Option<BeforeSend>,
    pub(crate) in_app_include: Vec<String>,
    pub(crate) in_app_exclude: Vec<String>,
    pub(crate) app_root: Option<PathBuf>,
    pub(crate) install_panic_hook: bool,
    pub(crate) batch_size: usize,
    pub(crate) flush_interval: Duration,
    pub(crate) max_queue_size: usize,
    pub(crate) shutdown_timeout: Duration,
    pub(crate) max_retries: u32,
    pub(crate) connect_timeout: Duration,
    pub(crate) timeout: Duration,
    pub(crate) performance_monitoring: bool,
    pub(crate) performance_traces: bool,
    pub(crate) performance_ignore_transactions: Vec<String>,
    #[cfg(feature = "performance")]
    pub(crate) sql_dialect: Option<crate::performance::Dialect>,
    pub(crate) debug: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    /// The defaults, ignoring environment variables. Without
    /// [`api_key`](Self::api_key) the client stays off.
    pub fn new() -> Self {
        Config {
            api_key: None,
            endpoint: DEFAULT_ENDPOINT.to_string(),
            environment: "production".to_string(),
            enabled: None,
            disabled_environments: vec!["development".into(), "test".into()],
            enabled_environments: None,
            release: None,
            server_name: None,
            filter_names: Vec::new(),
            filter_parts: DEFAULT_FILTER_PARTS.iter().map(|part| part.to_string()).collect(),
            send_request_params: false,
            send_request_headers: false,
            send_user: true,
            request_id_header: None,
            send_code_context: true,
            capture_server_errors: true,
            max_breadcrumbs: 40,
            tags: BTreeMap::new(),
            runtimes: Vec::new(),
            before_send: None,
            in_app_include: Vec::new(),
            in_app_exclude: Vec::new(),
            app_root: None,
            install_panic_hook: true,
            batch_size: 25,
            flush_interval: Duration::from_secs(2),
            max_queue_size: 1_000,
            shutdown_timeout: Duration::from_secs(5),
            max_retries: 3,
            connect_timeout: Duration::from_secs(2),
            timeout: Duration::from_secs(5),
            performance_monitoring: false,
            performance_traces: true,
            performance_ignore_transactions: Vec::new(),
            #[cfg(feature = "performance")]
            sql_dialect: None,
            debug: false,
        }
    }

    /// The defaults, then these environment variables:
    ///
    /// | Variable | Setting |
    /// |---|---|
    /// | `GOODSORCERY_API_KEY` | [`api_key`](Self::api_key) |
    /// | `GOODSORCERY_ENDPOINT` | [`endpoint`](Self::endpoint), default `https://app.goodsorcery.com` |
    /// | `GOODSORCERY_ENV` | [`environment`](Self::environment), default `production` |
    /// | `GOODSORCERY_ENABLED` | [`enabled`](Self::enabled) (`1`/`0`, `true`/`false`, `yes`/`no`, `on`/`off`) |
    /// | `GOODSORCERY_PERFORMANCE_MONITORING` | [`performance_monitoring`](Self::performance_monitoring), default off |
    /// | `GOODSORCERY_RELEASE`, `KAMAL_VERSION`, `HEROKU_SLUG_COMMIT`, `HEROKU_BUILD_COMMIT`, `RENDER_GIT_COMMIT`, `GIT_COMMIT`, `GIT_SHA`, `SOURCE_VERSION` | [`release`](Self::release): the first one set |
    /// | `GOODSORCERY_DEBUG` | [`debug`](Self::debug) |
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// [`from_env`](Self::from_env) with a custom lookup (used by tests,
    /// since changing the process environment is unsafe).
    pub(crate) fn from_vars(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| lookup(name).map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        let mut config = Config::new();
        config.api_key = get("GOODSORCERY_API_KEY");
        if let Some(endpoint) = get("GOODSORCERY_ENDPOINT") {
            config = config.endpoint(endpoint);
        }
        if let Some(environment) = get("GOODSORCERY_ENV") {
            config.environment = environment;
        }
        config.enabled = get("GOODSORCERY_ENABLED").and_then(|value| parse_flag(&value));
        if let Some(on) = get("GOODSORCERY_PERFORMANCE_MONITORING").and_then(|value| parse_flag(&value)) {
            config.performance_monitoring = on;
        }
        config.release = RELEASE_VARS.iter().find_map(|name| get(name)).map(|value| truncate(&value, 64));
        if let Some(on) = get("GOODSORCERY_DEBUG").and_then(|value| parse_flag(&value)) {
            config.debug = on;
        }
        config
    }

    /// The project environment's ingest key (`gs_live_…` or `gs_test_…`).
    /// It is never logged in full.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        let key = key.into().trim().to_string();
        self.api_key = if key.is_empty() { None } else { Some(key) };
        self
    }

    /// The server to report to, for a self-hosted instance
    /// (`https://errors.example.com`). It must be an `http://` or
    /// `https://` URL with a host and no `user:password@`, query or
    /// fragment; otherwise the client stays off and says why on stderr.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into().trim().trim_end_matches('/').to_string();
        self
    }

    /// This process's environment name, used only to decide whether the
    /// client is on ([`disabled_environments`](Self::disabled_environments)).
    /// The server takes the environment from the API key.
    pub fn environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = environment.into();
        self
    }

    /// Turns the client on or off regardless of the environment (it still
    /// needs an API key). Also `GOODSORCERY_ENABLED`.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    /// Environments in which the client stays off. Default `development`
    /// and `test`.
    pub fn disabled_environments<I, S>(mut self, environments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.disabled_environments = environments.into_iter().map(Into::into).collect();
        self
    }

    /// An allow-list: when set, the client is on only in these
    /// environments.
    pub fn enabled_environments<I, S>(mut self, environments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.enabled_environments = Some(environments.into_iter().map(Into::into).collect());
        self
    }

    /// The version or commit being run (up to 64 characters).
    pub fn release(mut self, release: impl Into<String>) -> Self {
        self.release = Some(truncate(&release.into(), 64));
        self
    }

    /// The host name sent with events and traces. Default: the `HOSTNAME`
    /// variable, else the system's host name where it can be read.
    pub fn server_name(mut self, name: impl Into<String>) -> Self {
        self.server_name = Some(truncate(&name.into(), 255));
        self
    }

    /// Adds key names whose values are replaced with `[FILTERED]` wherever
    /// they appear (context, tags, breadcrumb data, request params and
    /// headers). A name matches a whole key, ignoring case, with `-` and
    /// `_` treated alike.
    pub fn filter_keys<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.filter_names.extend(names.into_iter().map(Into::into));
        self
    }

    /// Adds key parts: any key containing one (ignoring case, `-` read as
    /// `_`) is filtered. The defaults are `passw`, `secret`, `token`,
    /// `_key`, `apikey`, `api_key`, `crypt`, `salt`, `certificate`, `otp`,
    /// `ssn`, `cvv`, `cvc`, `authorization`, `cookie`, `csrf`, `xsrf`,
    /// `sessionid`, `session_id`, `creditcard`, `credit_card`,
    /// `cardnumber`, `card_number`, `signature` and `email`.
    pub fn filter_key_parts<I, S>(mut self, parts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.filter_parts.extend(parts.into_iter().map(Into::into));
        self
    }

    /// Removes the default key parts, so only the names and parts you add
    /// are filtered. The server still scrubs its own list.
    pub fn without_default_filters(mut self) -> Self {
        self.filter_parts.retain(|part| !DEFAULT_FILTER_PARTS.contains(&part.as_str()));
        self
    }

    /// Sends a request's query parameters (filtered) with errors. Default
    /// off. Request bodies are never read.
    pub fn send_request_params(mut self, on: bool) -> Self {
        self.send_request_params = on;
        self
    }

    /// Sends request headers (filtered; `Cookie` and `Authorization` are
    /// always filtered) with errors. Default off.
    pub fn send_request_headers(mut self, on: bool) -> Self {
        self.send_request_headers = on;
        self
    }

    /// Sends the user set with [`set_user`](crate::set_user). Default on:
    /// nothing is sent unless the app sets a user.
    pub fn send_user(mut self, on: bool) -> Self {
        self.send_user = on;
        self
    }

    /// Reads the request id from this request header (for example
    /// `x-request-id`). Default none: the axum layer reads no headers.
    pub fn request_id_header(mut self, name: impl Into<String>) -> Self {
        self.request_id_header = Some(name.into().to_ascii_lowercase());
        self
    }

    /// Sends source lines around in-app frames when the source files can be
    /// read at run time. Default on.
    pub fn send_code_context(mut self, on: bool) -> Self {
        self.send_code_context = on;
        self
    }

    /// Reports 5xx responses from the axum layer as errors (unless an error
    /// was already captured for that request). Default on.
    pub fn capture_server_errors(mut self, on: bool) -> Self {
        self.capture_server_errors = on;
        self
    }

    /// Breadcrumbs kept per scope (0–100, default 40).
    pub fn max_breadcrumbs(mut self, max: usize) -> Self {
        self.max_breadcrumbs = max.min(100);
        self
    }

    /// A tag sent with every event.
    pub fn tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.tags.insert(key.into(), value.into());
        self
    }

    /// Adds a runtime to the app's label, after Rust (for example
    /// `.runtime("tokio", "1.41.0")`).
    pub fn runtime(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.runtimes.push((name.into(), version.into()));
        self
    }

    /// Called with each finished event (the JSON payload) on the sender
    /// thread, after filtering. Return `None` to drop it. A panic in the
    /// callback drops the event.
    pub fn before_send<F>(mut self, callback: F) -> Self
    where
        F: Fn(Value) -> Option<Value> + Send + Sync + 'static,
    {
        self.before_send = Some(Arc::new(callback));
        self
    }

    /// Crate names whose frames always count as the app's own code.
    pub fn in_app_include<I, S>(mut self, crates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.in_app_include.extend(crates.into_iter().map(Into::into));
        self
    }

    /// Crate names whose frames never count as the app's own code.
    pub fn in_app_exclude<I, S>(mut self, crates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.in_app_exclude.extend(crates.into_iter().map(Into::into));
        self
    }

    /// The directory the app was built from, stripped from frame paths.
    /// Usually detected from where [`init`](crate::init) is called.
    pub fn app_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.app_root = Some(root.into());
        self
    }

    /// Installs the panic hook that reports panics (default on). The
    /// previous hook still runs afterwards.
    pub fn install_panic_hook(mut self, on: bool) -> Self {
        self.install_panic_hook = on;
        self
    }

    /// Events per request (1–100, default 25).
    pub fn batch_size(mut self, size: usize) -> Self {
        self.batch_size = size.clamp(1, 100);
        self
    }

    /// The longest an event waits before it is sent (default 2 s).
    pub fn flush_interval(mut self, interval: Duration) -> Self {
        self.flush_interval = interval.max(Duration::from_millis(10));
        self
    }

    /// Events held in memory; beyond this new events are dropped and
    /// counted (default 1,000).
    pub fn max_queue_size(mut self, size: usize) -> Self {
        self.max_queue_size = size.max(1);
        self
    }

    /// How long dropping the guard waits to send what is queued (default
    /// 5 s).
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Retries for a batch on 5xx and network errors (default 3).
    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// TCP and TLS connect timeout (default 2 s).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Timeout for a whole request to the server (default 5 s).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Performance monitoring: response times, slow endpoints and traces of
    /// slow requests, sent once a minute. Default off; also
    /// `GOODSORCERY_PERFORMANCE_MONITORING`. Needs the `performance`
    /// feature (on by default).
    pub fn performance_monitoring(mut self, on: bool) -> Self {
        self.performance_monitoring = on;
        self
    }

    /// With performance monitoring on, also sends the slowest trace per
    /// transaction per minute (default on). `false` sends metrics only.
    pub fn performance_traces(mut self, on: bool) -> Self {
        self.performance_traces = on;
        self
    }

    /// Transaction names (exact, such as `GET /health`) never recorded.
    pub fn performance_ignore_transactions<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.performance_ignore_transactions.extend(names.into_iter().map(Into::into));
        self
    }

    /// The app's SQL dialect, for queries whose dialect is not known
    /// otherwise: sqlx query events (they don't say which database they
    /// come from), spans without `db.system`, and [`performance::record_sql`]
    /// or [`performance::record_span`] without one. It decides how quotes
    /// are read when literal values are removed: set it to
    /// [`Dialect::Mysql`](crate::performance::Dialect::Mysql) or
    /// [`Dialect::Sqlite`](crate::performance::Dialect::Sqlite) so a
    /// double-quoted value (`WHERE token = "a8f3k29dk3"`) is never mistaken
    /// for an identifier and kept.
    ///
    /// [`performance::record_sql`]: crate::performance::record_sql
    /// [`performance::record_span`]: crate::performance::record_span
    #[cfg(feature = "performance")]
    pub fn sql_dialect(mut self, dialect: crate::performance::Dialect) -> Self {
        self.sql_dialect = Some(dialect);
        self
    }

    /// Logs what the client does (on stderr). Default off: only problems are
    /// logged. Also `GOODSORCERY_DEBUG`.
    pub fn debug(mut self, on: bool) -> Self {
        self.debug = on;
        self
    }

    /// Whether the client would report with this configuration, and if not,
    /// why.
    pub(crate) fn gate(&self) -> Result<(), &'static str> {
        if self.api_key.is_none() {
            return Err("no API key is set");
        }
        self.environment_gate()?;
        if parse_endpoint(&self.endpoint).is_none() {
            return Err(BAD_ENDPOINT);
        }
        Ok(())
    }

    fn environment_gate(&self) -> Result<(), &'static str> {
        if let Some(enabled) = self.enabled {
            return if enabled { Ok(()) } else { Err("it is turned off") };
        }
        if let Some(allowed) = &self.enabled_environments {
            return if allowed.iter().any(|env| env == &self.environment) {
                Ok(())
            } else {
                Err("the environment is not in enabled_environments")
            };
        }
        if self.disabled_environments.iter().any(|env| env == &self.environment) {
            return Err("the environment is in disabled_environments");
        }
        Ok(())
    }

    /// The endpoint to send to (scheme lowercased), if it is valid.
    pub(crate) fn valid_endpoint(&self) -> Option<String> {
        parse_endpoint(&self.endpoint)
    }
}

/// Why the client is off when the endpoint is unusable.
pub(crate) const BAD_ENDPOINT: &str =
    "the endpoint must be an http:// or https:// URL with a host and no user:password, query or fragment";

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("api_key", &self.api_key.as_deref().map(masked_key))
            .field("endpoint", &self.valid_endpoint().unwrap_or_else(|| "(invalid)".into()))
            .field("environment", &self.environment)
            .field("enabled", &self.enabled)
            .field("release", &self.release)
            .field("performance_monitoring", &self.performance_monitoring)
            .finish_non_exhaustive()
    }
}

/// `1`, `true`, `yes`, `on` → true; `0`, `false`, `no`, `off` → false.
pub(crate) fn parse_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// An endpoint URL as it is sent to (lowercase scheme, no trailing slash),
/// or `None` unless it is `http://` or `https://` with a host and no
/// userinfo, query or fragment. A URL with any of those is refused rather
/// than rewritten: guessing where its host ends could send the API key to
/// a different host than the one meant.
pub(crate) fn parse_endpoint(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.chars().any(|c| c.is_whitespace() || c.is_control()) || endpoint.contains(['?', '#', '\\']) {
        return None;
    }
    let (scheme, rest) = endpoint.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    if authority.contains('@') || !valid_authority(authority) {
        return None;
    }
    Some(format!("{scheme}://{authority}{path}"))
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`, with a non-empty host.
fn valid_authority(authority: &str) -> bool {
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((host, after)) => match after {
                "" => (host, None),
                _ => (host, Some(after.strip_prefix(':').unwrap_or("x"))),
            },
            None => return false,
        },
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let host_ok =
        !host.is_empty() && !host.contains(['[', ']', '/']) && (authority.starts_with('[') || !host.contains(':'));
    let port_ok =
        port.is_none_or(|port| !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()));
    host_ok && port_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn from(vars: &[(&str, &str)]) -> Config {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_vars(|name| map.get(name).cloned())
    }

    #[test]
    fn reads_the_environment() {
        let config = from(&[
            ("GOODSORCERY_API_KEY", " gs_test_abc123456789 "),
            ("GOODSORCERY_ENDPOINT", "HTTPS://errors.example.com/"),
            ("GOODSORCERY_ENV", "staging"),
            ("GOODSORCERY_PERFORMANCE_MONITORING", "1"),
            ("GIT_SHA", "abc123"),
        ]);
        assert_eq!(config.api_key.as_deref(), Some("gs_test_abc123456789"));
        assert_eq!(config.valid_endpoint().as_deref(), Some("https://errors.example.com"));
        assert_eq!(config.environment, "staging");
        assert!(config.performance_monitoring);
        assert_eq!(config.release.as_deref(), Some("abc123"));
        assert!(config.gate().is_ok());
    }

    #[test]
    fn defaults() {
        let config = from(&[]);
        assert_eq!(config.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(config.environment, "production");
        assert!(!config.performance_monitoring);
        assert!(!config.send_request_headers);
        assert!(!config.send_request_params);
        assert_eq!(config.gate(), Err("no API key is set"));
    }

    #[test]
    fn release_vars_are_tried_in_order() {
        let config = from(&[("GIT_SHA", "late"), ("KAMAL_VERSION", "early")]);
        assert_eq!(config.release.as_deref(), Some("early"));
        let long = "x".repeat(100);
        assert_eq!(Config::new().release(long).release.unwrap().len(), 64);
    }

    #[test]
    fn environment_gating() {
        let keyed = Config::new().api_key("gs_test_123");
        assert!(keyed.clone().gate().is_ok());
        assert!(keyed.clone().environment("development").gate().is_err());
        assert!(keyed.clone().environment("development").enabled(true).gate().is_ok());
        assert!(keyed.clone().enabled(false).gate().is_err());
        assert!(keyed.clone().enabled_environments(["staging"]).gate().is_err());
        assert!(keyed.clone().environment("staging").enabled_environments(["staging"]).gate().is_ok());
        assert!(from(&[("GOODSORCERY_API_KEY", "k"), ("GOODSORCERY_ENABLED", "off")]).gate().is_err());
        assert!(Config::new().api_key("   ").gate().is_err());
    }

    #[test]
    fn debug_output_masks_the_key() {
        let text = format!("{:?}", Config::new().api_key("gs_live_9f2c0000e81a"));
        assert!(text.contains("gs_live_…e81a"));
        assert!(!text.contains("9f2c"));
    }

    #[test]
    fn endpoints_must_be_plain_http_urls() {
        for (given, sent) in [
            ("https://errors.example.com/", "https://errors.example.com"),
            ("HTTP://localhost:3000", "http://localhost:3000"),
            ("https://example.com/goodsorcery/", "https://example.com/goodsorcery"),
            ("http://[::1]:8080", "http://[::1]:8080"),
            ("  https://10.0.0.5  ", "https://10.0.0.5"),
        ] {
            assert_eq!(parse_endpoint(given).as_deref(), Some(sent), "{given}");
        }
        for refused in [
            "errors.example.com",
            "ftp://errors.example.com",
            "https://",
            "https:///path",
            "https://user:pw@errors.example.com",
            "https://user@errors.example.com",
            "https://host?x@evil.com",
            "https://host#@evil.com",
            "https://user:p/w@evil.com",
            "https://errors.example.com/?token=1",
            "https://errors.example.com:port",
            "https://errors.example.com:",
            "https://[::1",
            "https://a b.com",
            "https://evil.com\\@good.com",
        ] {
            assert_eq!(parse_endpoint(refused), None, "{refused}");
        }
    }

    #[test]
    fn an_invalid_endpoint_keeps_the_client_off() {
        let config = Config::new().api_key("gs_test_123").endpoint("https://user:pw@errors.example.com");
        assert_eq!(config.gate(), Err(BAD_ENDPOINT));
        assert!(!format!("{config:?}").contains("pw"));
        // Off for another reason first: no endpoint complaint.
        assert_eq!(config.clone().enabled(false).gate(), Err("it is turned off"));
        assert_eq!(Config::new().endpoint("nope").gate(), Err("no API key is set"));
    }

    #[test]
    fn flags() {
        assert_eq!(parse_flag("YES"), Some(true));
        assert_eq!(parse_flag("off"), Some(false));
        assert_eq!(parse_flag("maybe"), None);
    }
}

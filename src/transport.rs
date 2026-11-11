//! Talks to the ingest API: Bearer auth, JSON bodies gzip'd from 1 KiB,
//! responses read up to 64 KiB, outcomes classified for retries.
//!
//! The HTTP client is `ureq` (blocking, rustls, no async runtime) driven
//! from the client's own threads, so sending never depends on, or runs
//! inside, the app's runtime. Its agent keeps connections alive and is
//! shared by the error sender and the performance flusher.

use std::io::Write;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::Value;

use crate::runtime::{NOTIFIER_NAME, VERSION};
use crate::util::{log, masked_key};

pub(crate) const EVENTS_PATH: &str = "/api/v1/events";
pub(crate) const BATCH_PATH: &str = "/api/v1/events/batch";
pub(crate) const VERIFY_PATH: &str = "/api/v1/verify";
#[cfg(feature = "performance")]
pub(crate) const METRICS_PATH: &str = "/api/v1/apm/metrics";
#[cfg(feature = "performance")]
pub(crate) const TRACES_PATH: &str = "/api/v1/apm/traces";

const GZIP_MIN_BYTES: usize = 1_024;
const MAX_RESPONSE_BYTES: u64 = 65_536;
const DEFAULT_RETRY_AFTER: u64 = 60;
const TLS_ERROR: &str = "TLS error";

/// The outcome of one request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    /// 2xx, with the parsed body when it was small JSON.
    Success(Value),
    /// 404: an older server without this endpoint.
    NotFound,
    /// 429: wait this long before sending again.
    RateLimited(Duration),
    /// 5xx, 408, timeouts and connection errors.
    Retryable(String),
    /// A 3xx redirect (never followed, so the key is never forwarded) or any
    /// other 4xx (400, 401, 413, 415, 422 …): never retried.
    Rejected(u16),
}

impl Outcome {
    pub(crate) fn description(&self) -> String {
        match self {
            Outcome::Success(_) => "HTTP 2xx".into(),
            Outcome::NotFound => "HTTP 404".into(),
            Outcome::RateLimited(_) => "HTTP 429".into(),
            Outcome::Retryable(reason) => reason.clone(),
            Outcome::Rejected(status @ 300..=399) => {
                format!("HTTP {status} redirect (not followed: set the endpoint to the final https:// URL)")
            }
            Outcome::Rejected(status) => format!("HTTP {status}"),
        }
    }
}

/// Classifies a response status.
pub(crate) fn classify(status: u16, retry_after: Option<&str>, body: Value) -> Outcome {
    match status {
        200..=299 => Outcome::Success(body),
        404 => Outcome::NotFound,
        429 => Outcome::RateLimited(parse_retry_after(retry_after)),
        408 | 500..=599 => Outcome::Retryable(format!("HTTP {status}")),
        _ => Outcome::Rejected(status),
    }
}

/// `Retry-After` in seconds (an HTTP date counts as the default), clamped
/// to 1 s – 1 h; 60 s when missing.
pub(crate) fn parse_retry_after(value: Option<&str>) -> Duration {
    let seconds = value.and_then(|text| text.trim().parse::<u64>().ok()).unwrap_or(DEFAULT_RETRY_AFTER);
    Duration::from_secs(seconds.clamp(1, 3_600))
}

pub(crate) struct Transport {
    agent: ureq::Agent,
    endpoint: String,
    authorization: String,
    api_key: String,
}

impl Transport {
    pub(crate) fn new(endpoint: &str, api_key: &str, connect_timeout: Duration, timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .tls_config(tls_config())
            .timeout_connect(Some(connect_timeout))
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(format!("{NOTIFIER_NAME}/{VERSION}"))
            .build();
        warn_about_plain_http(endpoint);
        Transport {
            agent: ureq::Agent::new_with_config(config),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            authorization: format!("Bearer {api_key}"),
            api_key: api_key.to_string(),
        }
    }

    /// POSTs a JSON body, gzip'd when it is 1 KiB or more.
    pub(crate) fn post(&self, path: &str, json: &[u8]) -> Outcome {
        let mut request = self
            .agent
            .post(format!("{}{path}", self.endpoint))
            .header("Authorization", &self.authorization)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Accept-Encoding", "identity");
        let gzipped;
        let body = if json.len() >= GZIP_MIN_BYTES {
            match gzip(json) {
                Some(bytes) => {
                    gzipped = bytes;
                    request = request.header("Content-Encoding", "gzip");
                    &gzipped[..]
                }
                None => json,
            }
        } else {
            json
        };
        let outcome = self.finish(request.send(body));
        self.log_outcome(&outcome);
        outcome
    }

    /// `GET /api/v1/verify`.
    pub(crate) fn get(&self, path: &str) -> Outcome {
        let request = self
            .agent
            .get(format!("{}{path}", self.endpoint))
            .header("Authorization", &self.authorization)
            .header("Accept", "application/json")
            .header("Accept-Encoding", "identity");
        self.finish(request.call())
    }

    fn finish(&self, result: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Outcome {
        match result {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let retry_after =
                    response.headers().get("retry-after").and_then(|value| value.to_str().ok()).map(str::to_string);
                let body = response
                    .body_mut()
                    .with_config()
                    .limit(MAX_RESPONSE_BYTES)
                    .read_to_string()
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or(Value::Null);
                classify(status, retry_after.as_deref(), body)
            }
            Err(error) => Outcome::Retryable(error_kind(&error).to_string()),
        }
    }

    fn log_outcome(&self, outcome: &Outcome) {
        match outcome {
            Outcome::Rejected(401) => log::warn_once(
                "invalid_api_key",
                format!("invalid API key {} (HTTP 401); data is dropped", masked_key(&self.api_key)),
            ),
            Outcome::Rejected(status @ 300..=399) => log::warn_once(
                "redirected",
                format!(
                    "the endpoint redirected (HTTP {status}); redirects are not followed, so the API key is \
                     never forwarded and data is dropped: set GOODSORCERY_ENDPOINT (or Config::endpoint) to the \
                     final https:// URL"
                ),
            ),
            Outcome::Retryable(reason) if reason == TLS_ERROR => log::warn_once(
                "tls_error",
                if cfg!(feature = "platform-verifier") {
                    "TLS error talking to the endpoint (checked against the system certificate store); will retry"
                } else {
                    "TLS error talking to the endpoint (only the bundled Mozilla root certificates are trusted; \
                     the `platform-verifier` feature uses the system store); will retry"
                },
            ),
            Outcome::Rejected(status) => log::warn_every(
                "rejected",
                Duration::from_secs(60),
                format!("the server rejected a request (HTTP {status})"),
            ),
            Outcome::Retryable(reason) => log::debug(format!("request failed ({reason}); will retry")),
            _ => {}
        }
    }
}

/// A short, value-free name for a transport error (never the URL).
fn error_kind(error: &ureq::Error) -> &'static str {
    match error {
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::Io(io) if is_tls(io) => TLS_ERROR,
        ureq::Error::Io(_) => "connection error",
        ureq::Error::HostNotFound => "host not found",
        ureq::Error::ConnectionFailed => "connection failed",
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => TLS_ERROR,
        _ => "request error",
    }
}

/// rustls reports handshake and certificate failures (an unknown issuer, a
/// wrong host name) wrapped in an `io::Error`, so ureq hands them back as
/// `Error::Io`.
fn is_tls(error: &std::io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<rustls::Error>())
}

/// The bundled Mozilla roots (webpki-roots), or the operating system's
/// verifier and certificate store with the `platform-verifier` feature.
fn tls_config() -> ureq::tls::TlsConfig {
    let roots = if cfg!(feature = "platform-verifier") {
        ureq::tls::RootCerts::PlatformVerifier
    } else {
        ureq::tls::RootCerts::WebPki
    };
    ureq::tls::TlsConfig::builder().root_certs(roots).build()
}

fn gzip(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(bytes.len() / 4), Compression::fast());
    encoder.write_all(bytes).ok()?;
    encoder.finish().ok()
}

fn warn_about_plain_http(endpoint: &str) {
    let Some(rest) = endpoint.get(..7).filter(|scheme| scheme.eq_ignore_ascii_case("http://")).map(|_| &endpoint[7..])
    else {
        return;
    };
    let host = rest.split(['/', ':']).next().unwrap_or("");
    if !matches!(host, "localhost" | "127.0.0.1" | "[") && !rest.starts_with("[::1]") {
        log::warn_once("plain_http", "the endpoint uses plain HTTP; the API key is sent unencrypted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_statuses() {
        assert_eq!(classify(202, None, Value::Null), Outcome::Success(Value::Null));
        assert_eq!(classify(404, None, Value::Null), Outcome::NotFound);
        assert_eq!(classify(429, Some("7"), Value::Null), Outcome::RateLimited(Duration::from_secs(7)));
        assert_eq!(classify(429, None, Value::Null), Outcome::RateLimited(Duration::from_secs(60)));
        assert_eq!(classify(503, None, Value::Null), Outcome::Retryable("HTTP 503".into()));
        assert_eq!(classify(408, None, Value::Null), Outcome::Retryable("HTTP 408".into()));
        assert_eq!(classify(401, None, Value::Null), Outcome::Rejected(401));
        assert_eq!(classify(422, None, Value::Null), Outcome::Rejected(422));
        assert_eq!(classify(301, None, Value::Null), Outcome::Rejected(301));
    }

    #[test]
    fn redirects_are_described_with_the_fix() {
        let description = Outcome::Rejected(308).description();
        assert!(description.starts_with("HTTP 308 redirect"), "{description}");
        assert!(description.contains("final https:// URL"), "{description}");
        assert_eq!(Outcome::Rejected(422).description(), "HTTP 422");
    }

    #[test]
    fn a_failed_tls_handshake_is_a_tls_error() {
        use std::io::{Read, Write};
        // A plain-HTTP server behind an https:// endpoint: rustls rejects the reply.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.read(&mut [0; 1_024]);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
        });
        let endpoint = format!("https://127.0.0.1:{port}");
        let transport = Transport::new(&endpoint, "gs_live_test", Duration::from_secs(2), Duration::from_secs(5));
        assert_eq!(transport.get(VERIFY_PATH), Outcome::Retryable(TLS_ERROR.into()));
        server.join().unwrap();
    }

    #[test]
    fn retry_after_is_clamped() {
        assert_eq!(parse_retry_after(Some("0")), Duration::from_secs(1));
        assert_eq!(parse_retry_after(Some("99999")), Duration::from_secs(3_600));
        assert_eq!(parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")), Duration::from_secs(60));
    }

    #[test]
    fn gzip_round_trips() {
        use std::io::Read;
        let data = b"{\"a\":1}".repeat(500);
        let packed = gzip(&data).unwrap();
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&packed[..]).read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }
}

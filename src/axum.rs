//! The axum (tower) layer: one scope per request, errors for panics and 5xx
//! responses, and request timing for performance monitoring.
//!
//! ```no_run
//! use axum::{Router, routing::post};
//! # async fn checkout() {}
//! let app: Router = Router::new()
//!     .route("/checkout", post(checkout))
//!     .layer(goodsorcery::axum::layer());
//! ```
//!
//! Add it with `Router::layer` so it sees the matched route: each request is
//! named after its method and route pattern (`POST /checkout`,
//! `GET /users/{id}`), never the real path. It never reads request bodies,
//! headers or cookies; [`Config::send_request_headers`](crate::Config::send_request_headers),
//! [`send_request_params`](crate::Config::send_request_params) and
//! [`request_id_header`](crate::Config::request_id_header) opt in to more.
//!
//! A panic in a handler is reported (by the panic hook, with the request's
//! scope) and answered with an empty 500 response. A 5xx response is
//! reported unless an error was already captured for that request.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

use ::axum::extract::MatchedPath;
use ::axum::response::{IntoResponse, Response};
use http::{Request, StatusCode};
use pin_project_lite::pin_project;
use serde_json::{Map, Value};
use tower_layer::Layer;
use tower_service::Service;

use crate::client::{self, Client};
use crate::event::{ExceptionInfo, Options};
use crate::filter::{FILTERED, bound, query_params};
use crate::scope::{self, Hub};
use crate::util::{lock, truncate};

/// Headers whose values are never sent, whatever the filter settings.
const ALWAYS_FILTERED: &[&str] = &["cookie", "set-cookie", "authorization", "proxy-authorization"];

/// The layer: add it to a `Router` with `.layer(goodsorcery::axum::layer())`.
pub fn layer() -> GoodSorceryLayer {
    crate::runtime::note_axum();
    GoodSorceryLayer { _private: () }
}

/// See [`layer`].
#[derive(Debug, Clone)]
pub struct GoodSorceryLayer {
    _private: (),
}

impl<S> Layer<S> for GoodSorceryLayer {
    type Service = GoodSorceryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GoodSorceryService { inner }
    }
}

/// The service [`GoodSorceryLayer`] wraps around each route.
#[derive(Debug, Clone)]
pub struct GoodSorceryService<S> {
    inner: S,
}

impl<S, B> Service<Request<B>> for GoodSorceryService<S>
where
    S: Service<Request<B>>,
    S::Response: IntoResponse,
{
    type Response = Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let Some(client) = client::current_enabled() else {
            return ResponseFuture { inner: Some(self.inner.call(request)), state: None };
        };
        let state = RequestState::new(&client, &request);
        let inner = &mut self.inner;
        match scope::with_hub(state.hub.clone(), || catch_unwind(AssertUnwindSafe(|| inner.call(request)))) {
            Ok(future) => ResponseFuture { inner: Some(future), state: Some(state) },
            Err(payload) => {
                let mut state = state;
                state.panic_payload = Some(crate::panic::payload_message(&*payload));
                ResponseFuture { inner: None, state: Some(state) }
            }
        }
    }
}

pin_project! {
    /// The response future of [`GoodSorceryService`].
    pub struct ResponseFuture<F> {
        #[pin]
        inner: Option<F>,
        state: Option<RequestState>,
    }
}

impl<F, R, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<R, E>>,
    R: IntoResponse,
{
    type Output = Result<Response, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut slot = this.inner;
        let Some(state) = this.state.as_mut() else {
            return match slot.as_mut().as_pin_mut() {
                Some(inner) => inner.poll(cx).map(|result| result.map(IntoResponse::into_response)),
                None => Poll::Ready(Ok(StatusCode::INTERNAL_SERVER_ERROR.into_response())),
            };
        };
        let Some(inner) = slot.as_mut().as_pin_mut() else {
            return Poll::Ready(Ok(state.panicked()));
        };
        let hub = state.hub.clone();
        match scope::with_hub(hub.clone(), || catch_unwind(AssertUnwindSafe(|| inner.poll(cx)))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(Ok(response))) => {
                let response = scope::with_hub(hub, || response.into_response());
                state.finish(response.status().as_u16());
                Poll::Ready(Ok(response))
            }
            Ok(Poll::Ready(Err(error))) => {
                state.finish(500);
                Poll::Ready(Err(error))
            }
            Err(payload) => {
                slot.set(None);
                state.panic_payload = Some(crate::panic::payload_message(&*payload));
                Poll::Ready(Ok(state.panicked()))
            }
        }
    }
}

/// One request's scope, recorder and the bits needed to report it.
pub(crate) struct RequestState {
    hub: Hub,
    transaction: String,
    panic_payload: Option<String>,
    finished: bool,
}

impl RequestState {
    fn new<B>(client: &Client, request: &Request<B>) -> Self {
        let method = request.method().as_str().to_string();
        let path = request.uri().path().to_string();
        let route = request.extensions().get::<MatchedPath>().map(|matched| matched.as_str().to_string());
        let transaction = truncate(&format!("{method} {}", route.as_deref().unwrap_or(&path)), 1_000);
        let request_id = client
            .config
            .request_id_header
            .as_ref()
            .and_then(|name| request.headers().get(name.as_str()))
            .and_then(|value| value.to_str().ok())
            .map(|value| truncate(value, 128));
        let mut hub = scope::forked_hub(false);
        hub.reported = Some(Default::default());
        if let Some(scope) = &hub.scope {
            let mut data = lock(scope);
            data.transaction = Some(transaction.clone());
            data.request = Some(request_section(client, request, &method, request_id.clone()));
        }
        #[cfg(feature = "performance")]
        if let Some(route) = &route {
            hub.recorder = crate::performance::start_web(&transaction, &method, route, &path, request_id);
        }
        RequestState { hub, transaction, panic_payload: None, finished: false }
    }

    /// Records the outcome and reports a 5xx response.
    fn finish(&mut self, status: u16) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        if status >= 500 {
            self.report_server_error(status);
        }
        // After the report, so the trace links to its event.
        #[cfg(feature = "performance")]
        if let Some(recorder) = &self.hub.recorder {
            crate::performance::finish(recorder, Some(status), status >= 500);
        }
    }

    fn report_server_error(&self, status: u16) {
        let Some(client) = client::current_enabled() else { return };
        if client.config.capture_server_errors && !self.hub.has_reported() {
            let reason = StatusCode::from_u16(status).ok().and_then(|code| code.canonical_reason()).unwrap_or("");
            let info = ExceptionInfo {
                class: format!("HTTP {status}"),
                message: format!("{} responded with {status} {reason}", self.transaction).trim_end().to_string(),
                causes: Vec::new(),
            };
            scope::with_hub(self.hub.clone(), || client.capture(info, Options::default(), None, None));
        }
    }

    /// A handler panicked: make sure the panic was reported (the hook
    /// normally did, with a backtrace) and answer 500.
    fn panicked(&mut self) -> Response {
        let reported = crate::panic::take_last_panic().is_some() || self.hub.has_reported();
        if !reported {
            if let Some(client) = client::current_enabled() {
                let message = self.panic_payload.take().unwrap_or_default();
                let info = ExceptionInfo { class: "panic".into(), message, causes: Vec::new() };
                scope::with_hub(self.hub.clone(), || client.capture(info, Options::default(), None, None));
            }
        }
        if !self.finished {
            self.finished = true;
            #[cfg(feature = "performance")]
            if let Some(recorder) = &self.hub.recorder {
                crate::performance::finish(recorder, Some(500), true);
            }
        }
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

/// The event's `request` section: URL without query string, method,
/// request id, and (only when opted in) filtered query params and headers.
fn request_section<B>(client: &Client, request: &Request<B>, method: &str, request_id: Option<String>) -> Value {
    let uri = request.uri();
    let url = match (uri.scheme_str(), uri.authority()) {
        (Some(scheme), Some(authority)) => {
            let host = authority.as_str().rsplit_once('@').map_or(authority.as_str(), |(_, host)| host);
            format!("{scheme}://{host}{}", uri.path())
        }
        _ => uri.path().to_string(),
    };
    let mut section = Map::new();
    section.insert("url".into(), Value::String(truncate(&url, 4_096)));
    section.insert("method".into(), Value::String(method.to_string()));
    if let Some(id) = request_id {
        section.insert("request_id".into(), Value::String(id));
    }
    if client.config.send_request_params {
        if let Some(query) = uri.query() {
            let mut params = Value::Object(query_params(query));
            client.filter.filter(&mut params);
            bound(&mut params);
            section.insert("params".into(), params);
        }
    }
    if client.config.send_request_headers {
        let mut headers = Map::new();
        for (name, value) in request.headers() {
            let name = name.as_str();
            let value = if ALWAYS_FILTERED.contains(&name) {
                FILTERED.to_string()
            } else {
                match value.to_str() {
                    Ok(text) => text.to_string(),
                    Err(_) => continue,
                }
            };
            headers.insert(name.to_string(), Value::String(value));
        }
        client.filter.filter_map(&mut headers);
        let mut headers = Value::Object(headers);
        bound(&mut headers);
        section.insert("headers".into(), headers);
    }
    Value::Object(section)
}

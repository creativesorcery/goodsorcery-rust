//! Error tracking and performance monitoring for Rust apps, reporting to
//! [Good Sorcery](https://app.goodsorcery.com) (hosted or self-hosted).
//!
//! ```no_run
//! # #[cfg(feature = "axum")]
//! # mod example {
//! #[tokio::main]
//! async fn main() {
//!     // Reads GOODSORCERY_API_KEY, GOODSORCERY_ENV and GOODSORCERY_ENDPOINT,
//!     // and installs a panic hook that reports panics.
//!     let _guard = goodsorcery::init(
//!         goodsorcery::Config::from_env()
//!             // Performance monitoring: response times, slow endpoints, traces of slow requests.
//!             .performance_monitoring(true),
//!     );
//!
//!     let app: axum::Router = axum::Router::new()
//!         .route("/checkout", axum::routing::post(|| async { "ok" }))
//!         .layer(goodsorcery::axum::layer());
//!     # let _ = app;
//!     // Serve `app` here. Keep _guard until main returns:
//!     // dropping it sends anything still queued.
//! }
//! # }
//! # fn main() {}
//! ```
//!
//! What it does:
//!
//! - **Errors:** [`capture_error`] reports any `std::error::Error` (with its
//!   `source()` chain), [`capture_message`] a message, and the panic hook
//!   every panic. Events carry the current [scope](configure_scope): user,
//!   tags, context, transaction, fingerprint and [breadcrumbs](Breadcrumb).
//! - **Web:** [`axum::layer`] (feature `axum`) names each request after its
//!   route (`GET /users/{id}`), reports panics and 5xx responses with the
//!   method and path, and times requests when performance monitoring is on.
//!   It never reads request bodies, headers or cookies unless you opt in.
//! - **Jobs:** [`job`] runs a background job in its own scope and, with
//!   performance monitoring, records it.
//! - **Performance** (feature `performance`, off at run time until
//!   [`Config::performance_monitoring`]): per-minute histograms per
//!   transaction, the slowest trace per transaction per minute, database
//!   and HTTP time from [`tracing`] spans (sqlx, OpenTelemetry field names)
//!   or [`performance::record_span`].
//!
//! Everything is sent from a background thread with a blocking HTTP client,
//! so no particular async runtime is needed. Nothing in this crate panics
//! into your app: internal failures are caught and logged as one
//! `[goodsorcery]` line on stderr.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Helpers shared by the optional integrations go unused in reduced builds.
#![cfg_attr(
    not(all(feature = "axum", feature = "performance", feature = "tracing")),
    allow(dead_code, unused_mut, unused_imports, unused_variables)
)]

mod backtrace;
mod client;
mod config;
mod event;
mod filter;
mod job;
mod panic;
mod runtime;
mod scope;
mod transport;
mod util;
mod worker;

#[cfg(feature = "axum")]
pub mod axum;
#[cfg(feature = "performance")]
pub mod performance;
#[cfg(feature = "tracing")]
pub mod tracing;

pub use client::{ClientGuard, Verification, flush, init, is_enabled, verify};
pub use config::{Config, DEFAULT_ENDPOINT};
#[cfg(feature = "anyhow")]
pub use event::capture_anyhow;
pub use event::{EventId, Level, Options, capture_error, capture_error_with, capture_message, capture_message_with};
pub use job::{Job, JobOutcome, job};
pub use runtime::{NOTIFIER_NAME, VERSION};
pub use scope::{
    Breadcrumb, Scope, Scoped, User, add_breadcrumb, bind_current_scope, configure_scope, scoped, set_context, set_tag,
    set_transaction, set_user, with_scope,
};

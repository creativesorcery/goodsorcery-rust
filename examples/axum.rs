//! An axum app that reports an error, a panic and performance data.
//!
//! ```sh
//! GOODSORCERY_API_KEY=gs_test_… cargo run --example axum
//! # self-hosted: GOODSORCERY_ENDPOINT=http://localhost:3000, and PORT=4000 for this app
//! curl localhost:3001/error      # an error passed to capture_error (502)
//! curl localhost:3001/panic      # a panic in a handler (500)
//! curl -X POST localhost:3001/checkout   # queries (one N+1), an HTTP call, a job
//! ```
//!
//! Stop it with Ctrl-C: the guard sends what is queued, including this
//! minute's performance data.

use std::time::{Duration, SystemTime};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{get, post};
use goodsorcery::performance::{self, SpanKind};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

#[derive(Debug)]
struct ChargeError {
    order: u64,
}

impl std::fmt::Display for ChargeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the card for order {} was declined", self.order)
    }
}

impl std::error::Error for ChargeError {}

async fn charge_card(order: u64) -> Result<(), ChargeError> {
    performance::record_http("POST", "https://payments.example.com/v1/charges", Some(402), Duration::from_millis(120));
    Err(ChargeError { order })
}

async fn error() -> StatusCode {
    goodsorcery::set_user(Some(goodsorcery::User::new().id(5521)));
    if let Err(err) = charge_card(88123).await {
        tracing::warn!(order = 88123, "charge failed");
        goodsorcery::capture_error(&err);
        return StatusCode::BAD_GATEWAY;
    }
    StatusCode::OK
}

fn find_variant(id: u64) -> Option<&'static str> {
    (id < 4000).then_some("default")
}

async fn panic() -> &'static str {
    find_variant(4410).expect("variant 4410 is discontinued")
}

/// Loads each product with its own query: an N+1 the trace page flags.
fn load_products(ids: &[u64]) {
    for id in ids {
        performance::record_sql(
            &format!("SELECT * FROM products WHERE id = {id}"),
            Duration::from_millis(3),
            None,
            Some(1),
        );
    }
}

async fn checkout() -> &'static str {
    performance::record_span(SpanKind::Sql, Duration::from_millis(4), "SELECT * FROM carts WHERE token = 'c4rt-t0ken'");
    load_products(&(1..=12).collect::<Vec<_>>());
    // What sqlx logs for each query (seen by the goodsorcery tracing layer).
    tracing::debug!(
        target: "sqlx::query",
        summary = "UPDATE carts SET checked_out = true WHERE id = 88123",
        db.statement = "",
        rows_affected = 1u64,
        rows_returned = 0u64,
        elapsed_secs = 0.006f64,
        "query"
    );
    performance::record_http(
        "GET",
        "https://api.taxjar.example/v2/rates?zip=94107",
        Some(200),
        Duration::from_millis(80),
    );
    tokio::time::sleep(Duration::from_millis(150)).await;

    let enqueued = SystemTime::now();
    tokio::spawn(
        goodsorcery::job("SendReceipt", Some("mailers"), async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            load_products(&[1]);
            Ok::<(), std::io::Error>(())
        })
        .adapter("tokio")
        .enqueued_at(enqueued),
    );
    "checked out"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(goodsorcery::tracing::layer())
        .with(tracing_subscriber::fmt::layer().with_filter(LevelFilter::INFO))
        .init();

    // Reads GOODSORCERY_API_KEY, GOODSORCERY_ENV and GOODSORCERY_ENDPOINT,
    // and installs a panic hook that reports panics.
    let guard = goodsorcery::init(
        goodsorcery::Config::from_env()
            .environment("production")
            .release(env!("CARGO_PKG_VERSION"))
            .runtime("tokio", "1")
            .performance_monitoring(true),
    );
    if !guard.is_enabled() {
        eprintln!("GOODSORCERY_API_KEY is not set: nothing will be reported");
    } else {
        println!("verify: {:?}", tokio::task::spawn_blocking(goodsorcery::verify).await.unwrap());
    }

    let app = Router::new()
        .route("/error", get(error))
        .route("/panic", get(panic))
        .route("/checkout", post(checkout))
        .layer(goodsorcery::axum::layer());

    let port = std::env::var("PORT").unwrap_or_else(|_| "3001".into());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await.unwrap();
    println!("listening on http://{}", listener.local_addr().unwrap());
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .unwrap();
    // Dropping the guard sends what is still queued.
    drop(guard);
}

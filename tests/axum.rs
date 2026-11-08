//! The axum layer and performance monitoring, end to end.

#![cfg(all(feature = "axum", feature = "performance", feature = "tracing"))]
// The client is process-wide: `serial()` keeps these tests apart. Each runs
// on its own current-thread runtime, so holding the lock across awaits is safe.
#![allow(clippy::await_holding_lock)]

mod support;

use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use goodsorcery::performance::{self, SpanKind};
use serde_json::{Value, json};
use support::{FakeServer, Reply, config, serial};
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

async fn failing() -> (StatusCode, &'static str) {
    (StatusCode::INTERNAL_SERVER_ERROR, "nope")
}

async fn exploding() -> &'static str {
    panic!("handler exploded")
}

async fn capturing() -> StatusCode {
    goodsorcery::capture_error(&std::io::Error::other("charge failed"));
    StatusCode::BAD_GATEWAY
}

#[inline(never)]
fn load_products(count: u64) {
    for id in 0..count {
        performance::record_sql(
            &format!("SELECT * FROM products WHERE id = {id}"),
            Duration::from_millis(2),
            None,
            Some(1),
        );
    }
}

async fn checkout() -> &'static str {
    performance::record_span(
        SpanKind::Sql,
        Duration::from_millis(3),
        "SELECT * FROM users WHERE email = 'mara@example.com'",
    );
    load_products(12);
    performance::record_http(
        "GET",
        "https://user:pw@api.example.com/v2/rates?zip=94107",
        Some(200),
        Duration::from_millis(5),
    );
    tracing::debug!(
        target: "sqlx::query",
        summary = "SELECT 1 FROM carts WHERE token = 'cart-secret'",
        db.statement = "",
        rows_returned = 1u64,
        elapsed_secs = 0.004f64,
        "query"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    "ok"
}

fn app() -> Router {
    Router::new()
        .route("/users/{id}", get(failing))
        .route("/panic", get(exploding))
        .route("/capture", get(capturing))
        .route("/checkout", post(checkout))
        .route("/fine", get(|| async { "fine" }))
        .layer(goodsorcery::axum::layer())
}

async fn call(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)]) -> StatusCode {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap().status()
}

#[tokio::test]
async fn reports_5xx_responses_with_the_route_and_no_private_request_data() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    let app = app();
    let headers = [("cookie", "session=abc"), ("authorization", "Bearer xyz")];
    let status = call(&app, "GET", "/users/42?token=secret-token", &headers).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(call(&app, "GET", "/fine", &[]).await, StatusCode::OK);
    assert_eq!(call(&app, "GET", "/missing", &[]).await, StatusCode::NOT_FOUND);
    drop(guard);

    let events = server.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["exception"]["class"], "HTTP 500");
    assert_eq!(event["exception"]["message"], "GET /users/{id} responded with 500 Internal Server Error");
    assert_eq!(event["transaction"], "GET /users/{id}");
    assert_eq!(event["request"], json!({ "url": "/users/42", "method": "GET" }));
    let text = event.to_string();
    for secret in ["secret-token", "session=abc", "xyz"] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
}

#[tokio::test]
async fn a_panicking_handler_answers_500_and_is_reported_once() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    let app = app();
    assert_eq!(call(&app, "GET", "/panic", &[]).await, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(call(&app, "GET", "/capture", &[]).await, StatusCode::BAD_GATEWAY);
    drop(guard);

    let events = server.events();
    assert_eq!(events.len(), 2, "{events:#?}");
    let panic = &events[0];
    assert_eq!(panic["exception"]["class"], "panic");
    assert_eq!(panic["exception"]["message"], "handler exploded");
    assert_eq!(panic["transaction"], "GET /panic");
    assert_eq!(panic["request"]["url"], "/panic");
    assert_eq!(panic["exception"]["backtrace"][0]["file"], "tests/axum.rs");
    let captured = &events[1];
    assert_eq!(captured["exception"]["class"], "std::io::Error");
    assert_eq!(captured["transaction"], "GET /capture");
}

#[tokio::test]
async fn headers_params_and_request_ids_are_opt_in_and_filtered() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(
        config(&server)
            .send_request_headers(true)
            .send_request_params(true)
            .request_id_header("x-request-id")
            .without_default_filters(),
    );
    let app = app();
    let headers =
        [("cookie", "session=abc"), ("authorization", "Bearer xyz"), ("x-custom", "kept"), ("x-request-id", "req-7")];
    call(&app, "GET", "/users/42?page=2&token=t", &headers).await;
    drop(guard);

    let request = &server.events()[0]["request"];
    assert_eq!(request["request_id"], "req-7");
    assert_eq!(request["headers"]["cookie"], "[FILTERED]");
    assert_eq!(request["headers"]["authorization"], "[FILTERED]");
    assert_eq!(request["headers"]["x-custom"], "kept");
    assert_eq!(request["params"]["page"], "2");
    assert_eq!(request["params"]["token"], "t", "the default filters were removed");
}

#[tokio::test]
async fn records_requests_and_the_slowest_trace() {
    let _lock = serial();
    let server = FakeServer::start();
    let subscriber = tracing_subscriber::registry().with(goodsorcery::tracing::layer());
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let guard = goodsorcery::init(config(&server).performance_monitoring(true));
    let app = app();
    assert_eq!(call(&app, "POST", "/checkout?coupon=FALL25", &[]).await, StatusCode::OK);
    assert_eq!(call(&app, "GET", "/users/9", &[]).await, StatusCode::INTERNAL_SERVER_ERROR);
    drop(guard);

    let metrics = server.requests_to("/api/v1/apm/metrics");
    assert_eq!(metrics.len(), 1);
    let body = &metrics[0].body;
    assert_eq!(body["histogram"], "gs-v1");
    assert_eq!(body["notifier"]["name"], "goodsorcery-rust");
    assert_eq!(body["batch_id"].as_str().unwrap().len(), 36);
    let transactions: Vec<Value> = body["minutes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|minute| minute["transactions"].as_array().unwrap().clone())
        .collect();
    let checkout = transactions.iter().find(|t| t["name"] == "POST /checkout").expect("checkout metrics");
    assert_eq!(checkout["kind"], "web");
    assert_eq!(checkout["method"], "POST");
    assert_eq!(checkout["route"], "/checkout");
    assert_eq!(checkout["count"], 1);
    assert_eq!(checkout["failed"], 0);
    let bucket = &checkout["buckets"][0];
    assert_eq!(bucket[1], 1);
    assert!(bucket[2].as_f64().unwrap() >= 31.0, "db time {bucket}");
    assert!(bucket[3].as_f64().unwrap() >= 5.0, "http time {bucket}");
    let failing = transactions.iter().find(|t| t["name"] == "GET /users/{id}").expect("users metrics");
    assert_eq!(failing["failed"], 1);

    let traces = server.requests_to("/api/v1/apm/traces");
    assert_eq!(traces.len(), 1);
    let all = traces[0].body["traces"].as_array().unwrap();
    let trace = all.iter().find(|t| t["name"] == "POST /checkout").expect("checkout trace");
    assert_eq!(trace["status"], 200);
    assert_eq!(trace["request"], json!({ "method": "POST", "path": "/checkout" }));
    assert_eq!(trace["sql_count"], 14);
    assert_eq!(trace["http_count"], 1);
    assert!(trace["runtime"].as_array().unwrap().iter().any(|r| r["name"] == "axum"));
    let spans = trace["spans"].as_array().unwrap();
    let with_sql =
        |table: &str| spans.iter().find(|s| s["sql"].as_str().is_some_and(|sql| sql.contains(table))).cloned();
    let users = with_sql("users").unwrap();
    assert_eq!(users["sql"], "SELECT * FROM users WHERE email = ?");
    let products = with_sql("products").unwrap();
    assert_eq!(products["sql"], "SELECT * FROM products WHERE id = ?");
    assert_eq!(products["count"], 12);
    assert_eq!(products["rows"], 12);
    assert_eq!(products["call_site"]["file"], "tests/axum.rs");
    let context = products["call_site"]["context"]["lines"].as_array().unwrap();
    assert!(context.iter().any(|line| line.as_str().unwrap().contains("record_sql")));
    let carts = with_sql("carts").expect("the sqlx event");
    assert_eq!(carts["sql"], "SELECT ? FROM carts WHERE token = ?");
    let http = spans.iter().find(|s| s["type"] == "http").unwrap();
    assert_eq!(http["method"], "GET");
    assert_eq!(http["host"], "api.example.com");
    assert_eq!(http["path"], "/v2/rates");
    assert_eq!(http["status"], 200);

    let failed_trace = all.iter().find(|t| t["name"] == "GET /users/{id}").expect("users trace");
    assert_eq!(failed_trace["failed"], true);
    assert_eq!(failed_trace["status"], 500);
    let error_event = &server.events()[0];
    assert_eq!(failed_trace["error_event_id"], error_event["event_id"]);

    let everything: String = server.requests().iter().map(|r| r.text.clone()).collect();
    for secret in ["mara@example.com", "94107", "user:pw", "cart-secret", "FALL25"] {
        assert!(!everything.contains(secret), "{secret} leaked");
    }
}

#[tokio::test]
async fn jobs_are_recorded_with_their_queue_wait() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server).performance_monitoring(true));
    let enqueued = SystemTime::now() - Duration::from_millis(40);
    let result = goodsorcery::job("SendReceipt", Some("mailers"), async {
        load_products(2);
        goodsorcery::capture_message("receipt bounced", goodsorcery::Level::Warning);
        Err::<(), &str>("bounced")
    })
    .adapter("apalis")
    .enqueued_at(enqueued)
    .executions(2)
    .await;
    assert!(result.is_err());
    drop(guard);

    let event = &server.events()[0];
    assert_eq!(event["transaction"], "SendReceipt");
    assert_eq!(event["job"]["framework"], "apalis");
    assert_eq!(event["job"]["queue"], "mailers");
    assert_eq!(event["job"]["executions"], 2);

    let metrics = &server.requests_to("/api/v1/apm/metrics")[0].body;
    let job = &metrics["minutes"][0]["transactions"][0];
    assert_eq!(job["kind"], "job");
    assert_eq!(job["name"], "SendReceipt");
    assert_eq!(job["adapter"], "apalis");
    assert_eq!(job["queue"], "mailers");
    assert_eq!(job["failed"], 1);
    assert!(job["queue_wait_ms"].as_f64().unwrap() >= 40.0);
    let trace = &server.requests_to("/api/v1/apm/traces")[0].body["traces"][0];
    assert_eq!(trace["job"]["queue"], "mailers");
    assert_eq!(trace["job"]["executions"], 2);
    assert_eq!(trace["error_event_id"], event["event_id"]);
    assert!(trace.get("request").is_none());
}

#[tokio::test]
async fn a_server_without_performance_endpoints_turns_monitoring_off() {
    let _lock = serial();
    let server = FakeServer::start();
    server.respond_with(|request| {
        if request.path.starts_with("/api/v1/apm/") { Reply::status(404) } else { Reply::default_for(request) }
    });
    let guard = goodsorcery::init(config(&server).performance_monitoring(true));
    let app = app();
    call(&app, "GET", "/fine", &[]).await;
    assert!(goodsorcery::flush(Duration::from_secs(5)));
    call(&app, "GET", "/fine", &[]).await;
    assert!(goodsorcery::flush(Duration::from_secs(5)));
    drop(guard);
    assert_eq!(server.requests_to("/api/v1/apm/metrics").len(), 1);
    assert!(server.requests_to("/api/v1/apm/traces").is_empty());
}

#[tokio::test]
async fn performance_monitoring_is_off_by_default() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    let probe = || async { if performance::is_recording() { "on" } else { "off" } };
    let app = Router::new().route("/probe", get(probe)).layer(goodsorcery::axum::layer());
    let response = app.oneshot(Request::get("/probe").body(Body::empty()).unwrap()).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"off");
    drop(guard);
    assert!(server.requests().iter().all(|r| !r.path.starts_with("/api/v1/apm/")));
}

#[tokio::test]
async fn an_error_captured_in_a_child_scope_counts_for_the_request() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    let app = Router::new()
        .route(
            "/with-scope",
            get(|| async {
                goodsorcery::with_scope(|| goodsorcery::capture_error(&std::io::Error::other("in with_scope")));
                StatusCode::INTERNAL_SERVER_ERROR
            }),
        )
        .route(
            "/scoped",
            get(|| async {
                goodsorcery::scoped(async { goodsorcery::capture_error(&std::io::Error::other("in scoped")) }).await;
                StatusCode::INTERNAL_SERVER_ERROR
            }),
        )
        .route(
            "/inline-job",
            get(|| async {
                goodsorcery::job("Inline", None, async {
                    goodsorcery::capture_error(&std::io::Error::other("in a job"));
                })
                .await;
                StatusCode::INTERNAL_SERVER_ERROR
            }),
        )
        .layer(goodsorcery::axum::layer());
    for path in ["/with-scope", "/scoped", "/inline-job"] {
        assert_eq!(call(&app, "GET", path, &[]).await, StatusCode::INTERNAL_SERVER_ERROR);
    }
    drop(guard);

    let events = server.events();
    let messages: Vec<_> = events.iter().map(|event| event["exception"]["message"].clone()).collect();
    assert_eq!(messages, ["in with_scope", "in scoped", "in a job"], "no extra HTTP 500 events");
}

#[tokio::test]
async fn requests_never_take_a_user_set_outside_any_request() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    // No request scope is bound here: on the init thread the user goes to
    // the process-wide scope (allowed: a CLI app sets its user like this).
    goodsorcery::set_user(Some(goodsorcery::User::new().id(1).email("first@example.com")));
    goodsorcery::set_tag("region", "eu-west-1");
    // From any other thread with no scope bound (auth middleware outside
    // the layer, an unbound spawned task) it is refused.
    std::thread::spawn(|| {
        goodsorcery::set_user(Some(goodsorcery::User::new().id(2).email("second@example.com")));
        goodsorcery::capture_message("from an unbound thread", goodsorcery::Level::Error);
    })
    .join()
    .unwrap();
    let app = app();
    assert_eq!(call(&app, "GET", "/users/7", &[]).await, StatusCode::INTERNAL_SERVER_ERROR);
    goodsorcery::capture_message("from the init thread", goodsorcery::Level::Error);
    goodsorcery::set_user(None);
    goodsorcery::configure_scope(|scope| scope.remove_tag("region"));
    drop(guard);

    let events = server.events();
    assert_eq!(events.len(), 3);
    let unbound = &events[0];
    assert_eq!(unbound["exception"]["message"], "from an unbound thread");
    assert_eq!(unbound["user"], json!({ "id": "1", "email": "first@example.com" }), "the second user was refused");
    let request = &events[1];
    assert_eq!(request["exception"]["class"], "HTTP 500");
    assert!(request.get("user").is_none(), "{request}");
    assert_eq!(request["tags"]["region"], "eu-west-1", "tags are still inherited");
    assert_eq!(events[2]["user"]["id"], "1");
}

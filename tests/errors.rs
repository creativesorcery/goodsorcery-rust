//! Error events end to end, against a fake ingest server.

mod support;

use std::fmt;
use std::time::Duration;

use goodsorcery::{Breadcrumb, Level, Options, User};
use serde_json::{Value, json};
use support::{FakeServer, Reply, config, serial};

#[derive(Debug)]
struct CartError {
    source: std::io::Error,
}

impl fmt::Display for CartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("variant 4410 is discontinued")
    }
}

impl std::error::Error for CartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn cart_error() -> CartError {
    CartError { source: std::io::Error::other("disk full") }
}

#[test]
fn reports_an_error_with_its_causes_and_backtrace() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server).release("4f9e2c1"));
    assert!(guard.is_enabled());
    let id = goodsorcery::capture_error(&cart_error()).expect("an event id");
    drop(guard);

    let requests = server.requests_to("/api/v1/events/batch");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.header("authorization"), Some(format!("Bearer {}", support::API_KEY).as_str()));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("user-agent"), Some(format!("goodsorcery-rust/{}", goodsorcery::VERSION).as_str()));

    let event = &request.body["events"][0];
    assert_eq!(event["event_id"], id.to_string());
    assert_eq!(event["level"], "error");
    assert_eq!(event["release"], "4f9e2c1");
    assert_eq!(event["exception"]["class"], "errors::CartError");
    assert_eq!(event["exception"]["message"], "variant 4410 is discontinued");
    assert_eq!(event["context"]["cause"], json!([{ "class": "std::io::Error", "message": "disk full" }]));
    assert_eq!(event["context"]["runtime"][0]["name"], "Rust");
    assert_eq!(event["notifier"], json!({ "name": "goodsorcery-rust", "version": goodsorcery::VERSION }));

    let frames = event["exception"]["backtrace"].as_array().expect("frames");
    let top = &frames[0];
    assert_eq!(top["file"], "tests/errors.rs", "{top}");
    assert_eq!(top["in_app"], true);
    assert_eq!(top["method"], "reports_an_error_with_its_causes_and_backtrace");
    assert_eq!(top["module"], "errors");
    assert!(top["line"].as_u64().unwrap() > 0);
    assert!(top["column"].as_u64().unwrap() > 0);
    assert!(
        top["context"]["lines"].as_array().unwrap().iter().any(|line| line.as_str().unwrap().contains("capture_error"))
    );
    assert!(frames.iter().any(|frame| frame["in_app"] == false && frame.get("package").is_some()), "{frames:?}");
    assert!(!frames.iter().any(|frame| frame["module"].as_str().is_some_and(|m| m.starts_with("goodsorcery"))));
}

#[test]
fn events_carry_the_scope_and_options() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server).tag("region", "eu-west-1"));
    goodsorcery::with_scope(|| {
        goodsorcery::set_user(Some(User::new().id(5521).email("mara@example.com")));
        goodsorcery::set_tag("tenant", "acme");
        goodsorcery::set_transaction("nightly-report");
        goodsorcery::add_breadcrumb(Breadcrumb::new("custom", "Cart recalculated").data("items", 3));
        goodsorcery::configure_scope(|scope| scope.set_fingerprint(Some(["checkout", "missing-variant"])));
        goodsorcery::capture_error_with(&cart_error(), Options::new().level(Level::Warning).tag("disk", "data"));
    });
    goodsorcery::capture_message("Smoke test", Level::Info);
    drop(guard);

    let events = server.events();
    assert_eq!(events.len(), 2);
    let event = &events[0];
    assert_eq!(event["level"], "warning");
    assert_eq!(event["transaction"], "nightly-report");
    assert_eq!(event["user"], json!({ "id": "5521", "email": "mara@example.com" }));
    assert_eq!(event["tags"], json!({ "disk": "data", "region": "eu-west-1", "tenant": "acme" }));
    assert_eq!(event["fingerprint"], json!(["checkout", "missing-variant"]));
    assert_eq!(event["breadcrumbs"][0]["message"], "Cart recalculated");
    assert_eq!(event["breadcrumbs"][0]["data"], json!({ "items": 3 }));

    let message = &events[1];
    assert_eq!(message["exception"]["class"], "Message");
    assert_eq!(message["exception"]["message"], "Smoke test");
    assert_eq!(message["level"], "info");
    assert!(message.get("user").is_none(), "the scope stayed inside with_scope");
    assert!(message.get("transaction").is_none());
}

#[test]
fn calls_made_inside_configure_scope_do_not_deadlock() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    goodsorcery::with_scope(|| {
        goodsorcery::configure_scope(|scope| {
            scope.set_tag("outer", "yes");
            goodsorcery::add_breadcrumb(Breadcrumb::new("custom", "dropped: the scope is busy"));
            goodsorcery::set_tag("nested", "dropped");
            goodsorcery::capture_message("from inside configure_scope", Level::Error);
        });
        goodsorcery::capture_message("after", Level::Error);
    });
    drop(guard);
    let events = server.events();
    assert_eq!(events.len(), 2);
    assert!(events[0].get("tags").is_none());
    assert_eq!(events[1]["tags"], json!({ "outer": "yes" }));
    assert!(events[1].get("breadcrumbs").is_none());
}

#[test]
fn filters_sensitive_keys_before_sending() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server).filter_keys(["card_token"]).send_user(false));
    goodsorcery::with_scope(|| {
        goodsorcery::set_user(Some(User::new().email("mara@example.com")));
        goodsorcery::set_context(
            "checkout",
            json!({ "password": "hunter2", "nested": { "X-Api-Key": "k-123" }, "card_token": "tok_9", "cart_id": 88123 }),
        );
        goodsorcery::set_tag("auth_token", "t-abc");
        goodsorcery::add_breadcrumb(Breadcrumb::new("custom", "signed in").data("user_email", "mara@example.com"));
        goodsorcery::capture_error(&cart_error());
    });
    drop(guard);

    let event = &server.events()[0];
    let checkout = &event["context"]["checkout"];
    assert_eq!(checkout["password"], "[FILTERED]");
    assert_eq!(checkout["nested"]["X-Api-Key"], "[FILTERED]");
    assert_eq!(checkout["card_token"], "[FILTERED]");
    assert_eq!(checkout["cart_id"], 88123);
    assert_eq!(event["tags"]["auth_token"], "[FILTERED]");
    assert_eq!(event["breadcrumbs"][0]["data"]["user_email"], "[FILTERED]");
    assert!(event.get("user").is_none());
    // The backtrace's code context shows this file's own source lines.
    let mut without_source = event.clone();
    without_source["exception"]["backtrace"] = Value::Null;
    let text = without_source.to_string();
    for secret in ["hunter2", "k-123", "tok_9", "t-abc", "mara@example.com"] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
}

#[test]
fn before_send_can_change_or_drop_events() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server).before_send(|mut event: Value| {
        if event["exception"]["message"] == "drop me" {
            return None;
        }
        event["tags"] = json!({ "seen": "yes" });
        Some(event)
    }));
    goodsorcery::capture_message("drop me", Level::Error);
    goodsorcery::capture_message("keep me", Level::Error);
    drop(guard);

    let events = server.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["exception"]["message"], "keep me");
    assert_eq!(events[0]["tags"]["seen"], "yes");
}

#[test]
fn stays_off_without_a_key_or_in_a_disabled_environment() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(goodsorcery::Config::new().endpoint(&server.url));
    assert!(!guard.is_enabled());
    assert!(goodsorcery::capture_error(&cart_error()).is_none());
    drop(guard);

    let guard = goodsorcery::init(config(&server).environment("development"));
    assert!(!goodsorcery::is_enabled());
    assert!(goodsorcery::capture_message("nope", Level::Error).is_none());
    assert_eq!(goodsorcery::verify(), goodsorcery::Verification::Disabled);
    drop(guard);

    std::thread::sleep(Duration::from_millis(100));
    assert!(server.requests().is_empty());
}

#[test]
fn an_invalid_key_is_not_retried() {
    let _lock = serial();
    let server = FakeServer::start();
    server.respond_with(|_| Reply::status(401).body(json!({ "error": "invalid_api_key" })));
    let guard = goodsorcery::init(config(&server));
    goodsorcery::capture_error(&cart_error());
    assert!(goodsorcery::flush(Duration::from_secs(5)));
    drop(guard);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn rate_limits_are_waited_out_and_retries_keep_the_event_id() {
    let _lock = serial();
    let server = FakeServer::start();
    let mut calls = 0;
    server.respond_with(move |request| {
        calls += 1;
        match calls {
            1 => Reply::status(429).header("Retry-After", "1").body(json!({ "error": "rate_limited" })),
            2 => Reply::status(503),
            _ => Reply::default_for(request),
        }
    });
    let guard = goodsorcery::init(config(&server));
    let id = goodsorcery::capture_error(&cart_error()).unwrap();
    let requests = server.wait_for(3, Duration::from_secs(10));
    drop(guard);
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.body["events"][0]["event_id"], id.to_string());
    }
}

#[test]
fn falls_back_to_single_events_without_a_batch_endpoint() {
    let _lock = serial();
    let server = FakeServer::start();
    server.respond_with(|request| match request.path.as_str() {
        "/api/v1/events/batch" => Reply::status(404),
        _ => Reply::default_for(request),
    });
    let guard = goodsorcery::init(config(&server).flush_interval(Duration::from_millis(300)));
    goodsorcery::capture_message("one", Level::Error);
    goodsorcery::capture_message("two", Level::Error);
    drop(guard);
    let singles = server.requests_to("/api/v1/events");
    assert_eq!(singles.len(), 2);
    assert_eq!(singles[0].body["exception"]["message"], "one");
}

#[test]
fn splits_a_batch_refused_as_too_large() {
    let _lock = serial();
    let server = FakeServer::start();
    server.respond_with(|request| match request.body["events"].as_array().map(Vec::len) {
        Some(count) if count > 1 => Reply::status(413),
        _ => Reply::default_for(request),
    });
    let guard = goodsorcery::init(config(&server).flush_interval(Duration::from_millis(300)));
    goodsorcery::capture_message("one", Level::Error);
    goodsorcery::capture_message("two", Level::Error);
    drop(guard);
    let sizes: Vec<usize> = server.requests().iter().map(|r| r.body["events"].as_array().map_or(0, Vec::len)).collect();
    assert_eq!(sizes, [2, 1, 1]);
}

#[test]
fn large_bodies_are_gzipped() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    goodsorcery::capture_error_with(&cart_error(), Options::new().context("notes", "x".repeat(4_000)));
    drop(guard);
    let request = &server.requests()[0];
    assert_eq!(request.header("content-encoding"), Some("gzip"));
    assert_eq!(request.body["events"][0]["context"]["notes"].as_str().unwrap().len(), 4_000);
}

#[test]
fn verifies_the_key() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    assert_eq!(goodsorcery::verify(), goodsorcery::Verification::Ok { environment: "staging".into() });
    server.respond_with(|_| Reply::status(401));
    assert_eq!(goodsorcery::verify(), goodsorcery::Verification::InvalidKey);
    // Redirects are never followed (the key would go with them); the result
    // says what to change.
    server.respond_with(|_| Reply::status(301).header("Location", "https://elsewhere.example.com/api/v1/verify"));
    let goodsorcery::Verification::Failed(reason) = goodsorcery::verify() else { panic!("expected Failed") };
    assert!(reason.starts_with("HTTP 301 redirect"), "{reason}");
    assert!(reason.contains("final https:// URL"), "{reason}");
    drop(guard);
}

#[cfg(feature = "anyhow")]
#[test]
fn reports_anyhow_errors_with_their_chain() {
    use anyhow::Context;
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    let error = std::fs::read("/no/such/goodsorcery/file").context("loading the price list").unwrap_err();
    goodsorcery::capture_anyhow(&error);
    drop(guard);
    let event = &server.events()[0];
    assert_eq!(event["exception"]["class"], "std::io::Error");
    assert_eq!(event["exception"]["message"], "loading the price list");
    assert_eq!(event["context"]["cause"][0]["class"], "std::io::Error");
    assert_eq!(event["exception"]["backtrace"][0]["file"], "tests/errors.rs");
}

#[test]
fn trait_objects_report_a_readable_class() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    let error: Box<dyn std::error::Error + Send + Sync> =
        Box::new(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
    goodsorcery::capture_error(&*error);
    let parse = "x".parse::<u32>().unwrap_err();
    goodsorcery::capture_error(&parse);
    drop(guard);
    let events = server.events();
    assert_eq!(events[0]["exception"]["message"], "gone");
    assert_eq!(events[1]["exception"]["class"], "core::num::error::ParseIntError");
}

#[test]
fn duration_max_means_wait_as_long_as_it_takes() {
    let _lock = serial();
    let server = FakeServer::start();
    let guard = goodsorcery::init(
        config(&server).flush_interval(Duration::MAX).shutdown_timeout(Duration::MAX).performance_monitoring(true),
    );
    goodsorcery::capture_message("first", Level::Error);
    assert!(goodsorcery::flush(Duration::MAX));
    assert_eq!(server.events().len(), 1);
    goodsorcery::capture_message("second", Level::Error);
    assert!(guard.flush(Duration::MAX));
    goodsorcery::capture_message("third", Level::Error);
    drop(guard);
    assert_eq!(server.events().len(), 3);
}

#[test]
fn shutdown_waits_at_most_the_timeout_in_all() {
    let _lock = serial();
    let server = FakeServer::start();
    server.respond_with(|request| {
        std::thread::sleep(Duration::from_secs(3));
        Reply::default_for(request)
    });
    let guard = goodsorcery::init(
        config(&server)
            .performance_monitoring(true)
            .timeout(Duration::from_secs(10))
            .shutdown_timeout(Duration::from_millis(800)),
    );
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    runtime.block_on(goodsorcery::job("NightlyReport", None, async {}));
    goodsorcery::capture_message("slow server", Level::Error);
    // Let the worker start sending, so both the error and the performance
    // data are stuck on the slow server at shutdown.
    std::thread::sleep(Duration::from_millis(200));
    let started = std::time::Instant::now();
    drop(guard);
    let waited = started.elapsed();
    assert!(waited < Duration::from_millis(1_300), "shutdown took {waited:?}");
}

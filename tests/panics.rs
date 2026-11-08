//! The panic hook (its own test binary: the hook is process-wide).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use support::{FakeServer, config};

#[inline(never)]
fn explode(order: u32) {
    panic!("boom {order}");
}

#[test]
fn reports_panics_and_keeps_the_previous_hook() {
    let previous_calls = Arc::new(AtomicUsize::new(0));
    let counter = previous_calls.clone();
    // Count this test's own panics; anything else (a failed assertion) goes to
    // the default hook so CI shows why the test failed.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info.payload().downcast_ref::<String>().map(String::as_str);
        if message.is_some_and(|message| message.starts_with("boom ")) {
            counter.fetch_add(1, Ordering::SeqCst);
        } else {
            default_hook(info);
        }
    }));

    let server = FakeServer::start();
    let guard = goodsorcery::init(config(&server));
    let result = std::thread::Builder::new()
        .name("checkout-worker".into())
        .spawn(|| {
            goodsorcery::set_tag("order", 42);
            explode(42)
        })
        .unwrap()
        .join();
    assert!(result.is_err());
    drop(guard);

    assert_eq!(previous_calls.load(Ordering::SeqCst), 1, "the previous hook still runs");
    let events = server.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["exception"]["class"], "panic");
    assert_eq!(event["exception"]["message"], "boom 42");
    assert_eq!(event["level"], "error");
    assert_eq!(event["context"]["panic"]["thread"], "checkout-worker");
    assert!(event["context"]["panic"]["location"].as_str().unwrap().starts_with("tests/panics.rs:"));
    let top = &event["exception"]["backtrace"][0];
    assert_eq!(top["file"], "tests/panics.rs", "{top}");
    assert_eq!(top["method"], "explode");
    assert_eq!(top["in_app"], true);

    // A panic inside a configure_scope closure (the scope is locked) is
    // still reported, without deadlocking on the scope.
    let guard = goodsorcery::init(config(&server));
    let result = std::thread::spawn(|| goodsorcery::configure_scope(|_| explode(9))).join();
    assert!(result.is_err());
    drop(guard);
    let events = server.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["exception"]["message"], "boom 9");

    // A panic raised inside std: its location is shown like a frame path,
    // never the toolchain's absolute path.
    let guard = goodsorcery::init(config(&server));
    let result = std::thread::spawn(|| std::time::Instant::now() + std::time::Duration::MAX).join();
    assert!(result.is_err());
    drop(guard);
    let events = server.events();
    assert_eq!(events.len(), 3);
    let location = events[2]["context"]["panic"]["location"].as_str().unwrap();
    assert!(location.starts_with("rust/library/std/src/time.rs:"), "{location}");

    // With the client gone, panics are no longer reported.
    let _ = std::thread::spawn(|| explode(7)).join();
    assert_eq!(server.events().len(), 3);
}

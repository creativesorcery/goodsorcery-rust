//! Breadcrumbs from tracing events.

#![cfg(feature = "tracing")]

mod support;

use support::{FakeServer, config, serial};
use tracing_subscriber::prelude::*;

#[test]
fn tracing_events_become_filtered_breadcrumbs() {
    let _lock = serial();
    let server = FakeServer::start();
    let subscriber = tracing_subscriber::registry().with(goodsorcery::tracing::layer());
    let guard = goodsorcery::init(config(&server));
    tracing::subscriber::with_default(subscriber, || {
        goodsorcery::with_scope(|| {
            tracing::debug!("too chatty");
            tracing::info!(items = 3, user_email = "mara@example.com", "Cart {} recalculated", 88123);
            tracing::warn!("Cart contains a discontinued variant");
            goodsorcery::capture_message("checkout failed", goodsorcery::Level::Error);
        });
    });
    drop(guard);

    let event = &server.events()[0];
    let crumbs = event["breadcrumbs"].as_array().unwrap();
    assert_eq!(crumbs.len(), 2, "{crumbs:?}");
    assert_eq!(crumbs[0]["category"], "log");
    assert_eq!(crumbs[0]["level"], "info");
    assert_eq!(crumbs[0]["message"], "Cart 88123 recalculated");
    assert_eq!(crumbs[0]["data"]["items"], 3);
    assert_eq!(crumbs[0]["data"]["user_email"], "[FILTERED]");
    assert_eq!(crumbs[1]["level"], "warning");
}

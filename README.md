# goodsorcery (Rust client)

Error tracking and performance monitoring for Rust apps, reporting to [Good Sorcery](https://app.goodsorcery.com), hosted or self-hosted.

- **Errors:** panics anywhere in the process, any `std::error::Error` you pass to `capture_error` (with its `source()` chain), messages, and `anyhow::Error`s.
- **Web:** an axum (tower) layer that names each request after its route (`GET /users/{id}`), reports panics and 5xx responses with the method and path, and times requests.
- **Performance** (opt-in): response times and slow endpoints per minute, and a trace of the slowest request per endpoint per minute, with database and HTTP time, normalised SQL and N+1 detection.

It speaks the Good Sorcery ingest protocol ([`INGEST_API.md`](https://github.com/creativesorcery/goodsorcery_web/blob/main/docs/INGEST_API.md), revision 1.1). Everything is sent from a background thread with a blocking HTTP client, so it works with any async runtime (or none), and nothing in this crate panics into your app.

MSRV: Rust 1.85.

## Install

The crate is not on crates.io. Add it from GitHub:

```sh
cargo add goodsorcery --git https://github.com/creativesorcery/goodsorcery-rust
```

```toml
[dependencies]
goodsorcery = { git = "https://github.com/creativesorcery/goodsorcery-rust", branch = "main" }  # or pin a `rev`
```

Features (default: `axum`, `performance`, `tracing`):

| Feature | What it adds |
|---|---|
| `axum` | `goodsorcery::axum::layer()` |
| `performance` | performance monitoring (still off at run time until you turn it on) |
| `tracing` | `goodsorcery::tracing::layer()`: breadcrumbs from tracing events, sqlx and OpenTelemetry-style spans as timings |
| `anyhow` | `goodsorcery::capture_anyhow` |

## Set up

```sh
export GOODSORCERY_API_KEY=gs_live_…                      # from Settings › Setup & API keys
export GOODSORCERY_ENDPOINT=https://errors.example.com    # self-hosted only
```

```rust
#[tokio::main]
async fn main() {
    // Reads GOODSORCERY_API_KEY, GOODSORCERY_ENV and GOODSORCERY_ENDPOINT,
    // and installs a panic hook that reports panics.
    let _guard = goodsorcery::init(
        goodsorcery::Config::from_env()
            // Performance monitoring: response times, slow endpoints, traces of slow requests.
            // .performance_monitoring(true)
    );
    // Build and serve your app here. Keep _guard until main returns:
    // dropping it sends anything still queued.
}
```

Call `init` once, first thing in `main`. **Without an API key the client stays off**: nothing is captured, no thread starts, and every call returns at once, so development builds send nothing unless you set a key.

Check the key and endpoint without sending an event (it blocks while it asks the server):

```rust
match goodsorcery::verify() {
    goodsorcery::Verification::Ok { environment } => println!("reporting to {environment}"),
    other => eprintln!("Good Sorcery is not reporting: {other:?}"),
}
```

Cast a test error:

```rust
goodsorcery::capture_error(&std::io::Error::other("Good Sorcery test error"));
```

## Configuration

`Config::from_env()` reads:

| Variable | Setting | Default |
|---|---|---|
| `GOODSORCERY_API_KEY` | `.api_key(…)` | none (client off) |
| `GOODSORCERY_ENDPOINT` | `.endpoint(…)` | `https://app.goodsorcery.com` |
| `GOODSORCERY_ENV` | `.environment(…)` | `production` |
| `GOODSORCERY_ENABLED` | `.enabled(…)` (`1`/`0`, `true`/`false`, `yes`/`no`, `on`/`off`) | unset |
| `GOODSORCERY_PERFORMANCE_MONITORING` | `.performance_monitoring(…)` | off |
| `GOODSORCERY_RELEASE`, `KAMAL_VERSION`, `HEROKU_SLUG_COMMIT`, `HEROKU_BUILD_COMMIT`, `RENDER_GIT_COMMIT`, `GIT_COMMIT`, `GIT_SHA`, `SOURCE_VERSION` | `.release(…)` (the first one set) | none |
| `GOODSORCERY_DEBUG` | `.debug(…)` | off |

The client is on when it has a key and, in order: `.enabled(…)` (or `GOODSORCERY_ENABLED`) says so; else the environment is in `.enabled_environments(…)` when that is set; else the environment is not in `.disabled_environments(…)` (default `development` and `test`). The server takes the environment from the API key; `GOODSORCERY_ENV` only decides whether this process reports.

Other builder methods (all optional):

```rust
goodsorcery::Config::from_env()
    .release(env!("CARGO_PKG_VERSION"))
    .tag("region", "eu-west-1")                 // sent with every event
    .runtime("tokio", "1.41")                   // added to the app's label after Rust
    .server_name("api-3")                       // default: HOSTNAME or the system host name
    .filter_keys(["card_token"])                // more keys to filter (see Privacy)
    .before_send(|event| Some(event))           // change or drop events (None drops)
    .max_breadcrumbs(40)
    .send_code_context(true)                    // source lines around in-app frames, when readable
    .in_app_include(["shop_core"])              // crates that count as your code
    .capture_server_errors(true)                // report 5xx responses from the axum layer
    .performance_ignore_transactions(["GET /health"])
    .shutdown_timeout(std::time::Duration::from_secs(5));
```

Delivery tuning: `.batch_size(25)`, `.flush_interval(2 s)`, `.max_queue_size(1_000)`, `.max_retries(3)`, `.connect_timeout(2 s)`, `.timeout(5 s)`.

## axum

```rust
let app = axum::Router::new()
    .route("/checkout", axum::routing::post(checkout))
    .layer(goodsorcery::axum::layer());
```

Add it with `Router::layer` so it sees the matched route. For each request it:

- opens a fresh scope, so errors carry the transaction (`POST /checkout`, the route **pattern**, never the real path) and a request section: the URL without query string and the method;
- reports a panic in a handler (through the panic hook, with a backtrace) and answers an empty 500;
- reports a 5xx response, unless an error was already captured for that request;
- with performance monitoring on, times the request.

It never reads request bodies, headers or cookies. To send more, opt in: `.send_request_params(true)` (query parameters, filtered), `.send_request_headers(true)` (filtered; `Cookie` and `Authorization` always show as `[FILTERED]`) and `.request_id_header("x-request-id")`.

A task spawned from a handler keeps the request's scope when you wrap it:

```rust
tokio::spawn(goodsorcery::bind_current_scope(async move { send_receipt().await }));
```

## Errors

```rust
if let Err(err) = charge_card(&order).await {
    goodsorcery::capture_error(&err);
    return Err(err);
}
```

`capture_error` takes any `std::error::Error` (also `&*boxed` trait objects), never panics or blocks, and sends in the background. It returns the event id, or `None` when the client is off. The event has:

- `exception.class`: the error's type (`shop::cart::CartError`, `std::io::Error`); for a trait object, the leading name of its `Debug` output;
- `exception.message`: its `Display`;
- up to three causes from `source()`, under `context.cause`;
- a backtrace of the call site, resolved on the sender thread: app-relative files, line and column, function and module, `in_app` for your own crates and `package` for dependencies.

More:

```rust
use goodsorcery::{Level, Options, User};

goodsorcery::capture_error_with(&err, Options::new()
    .level(Level::Warning)
    .tag("payment_provider", "stripe")
    .context("order", serde_json::json!({ "id": 88123 }))
    .fingerprint(["payments", "card-declined"]));   // custom grouping
goodsorcery::capture_message("Cart total went negative", Level::Warning);
goodsorcery::capture_anyhow(&error);                 // feature "anyhow"
```

**Panics:** `init` installs a panic hook (keeping the previous one, which still runs). A panic is reported with class `panic`, its message, the thread and location, and the panicking thread's backtrace and scope. With `panic = "abort"` the hook also sends the event before the process aborts.

**Scopes:** what goes with the errors of one unit of work. The axum layer and `goodsorcery::job` give each request or job run its own scope; outside them changes go to the process-wide scope. Requests and jobs take tags and context from the process-wide scope but never its user, and `set_user` outside a request or job is ignored (with a warning) except on the thread that called `init`: on the process-wide scope the user would go with other users' errors. So middleware that sets the user must sit inside the goodsorcery layer (add `goodsorcery::axum::layer()` last: `.layer(auth).layer(goodsorcery::axum::layer())`), and a task spawned from a handler should be wrapped in `goodsorcery::bind_current_scope`.

```rust
goodsorcery::set_user(Some(User::new().id(5521).email("mara@example.com")));
goodsorcery::set_tag("tenant", "acme");
goodsorcery::set_context("cart", serde_json::json!({ "items": 3 }));
goodsorcery::set_transaction("nightly-report");
goodsorcery::configure_scope(|scope| scope.set_fingerprint(Some(["checkout"])));
goodsorcery::with_scope(|| { /* changes made here stay here */ });
let task = goodsorcery::scoped(async { /* its own copy of the scope */ });
```

**Breadcrumbs:** `goodsorcery::add_breadcrumb(Breadcrumb::new("custom", "Cart recalculated").data("items", 3))`, or let the tracing layer turn events into breadcrumbs:

```rust
use tracing_subscriber::prelude::*;
tracing_subscriber::registry()
    .with(goodsorcery::tracing::layer())          // .breadcrumb_level(tracing::Level::WARN)
    .with(tracing_subscriber::fmt::layer())
    .init();
```

Events at `INFO` and above become `log` breadcrumbs with their message and fields.

**Background jobs:**

```rust
goodsorcery::job("SendReceipt", Some("mailers"), send_receipt(order_id))
    .adapter("apalis")
    .enqueued_at(enqueued_at)   // its queue wait is measured from here
    .await?;
```

Errors and panics inside carry the job's name as the transaction and a `job` section. An `Err` output marks the run failed in performance metrics; capture the error itself with `capture_error` if you want an issue for it.

## Performance monitoring

Off by default. Turn it on with `.performance_monitoring(true)` (or `GOODSORCERY_PERFORMANCE_MONITORING=1`) and add the axum layer. Requests and jobs are aggregated in process and sent once a minute: per transaction, a count, failures, a duration histogram (scheme `gs-v1`) and time per layer (database, external HTTP, rendering and app code, which is the remainder), plus a trace of the slowest request per transaction per minute.

Database and HTTP time come from:

- **sqlx**, through the tracing layer: sqlx logs each query as a `DEBUG` event on `sqlx::query`. Make sure that event reaches the goodsorcery layer: filter your other layers individually (`fmt::layer().with_filter(EnvFilter::from_default_env())`), not with a global filter that drops `DEBUG`. sqlx's events don't say which database they come from, so tell the client: `.sql_dialect(Dialect::Postgres)` (or `Mysql`, `Sqlite`, from `goodsorcery::performance::Dialect`). It decides how quotes are read when values are removed from the SQL; without it a double-quoted MySQL or SQLite value made only of letters and digits is taken for an identifier and kept. SQLite runs each connection's queries on its own thread, inside the caller's span: the axum layer and `goodsorcery::job` open a `TRACE` span (target `goodsorcery`) per request and job run so those queries are linked back. A global filter that drops it leaves SQLite queries untimed (add `goodsorcery=trace` to a global `EnvFilter`, or filter per layer).
- **Spans with OpenTelemetry field names**, through the tracing layer: `db.statement` or `db.query.text` (with `db.system` for the SQL dialect) for queries; `http.request.method` and `url.full` (or `http.method` and `http.url`), with `http.response.status_code`, for outgoing HTTP calls (as `reqwest-tracing` records them).
- **Your own code**:

```rust
use goodsorcery::performance::{self, SpanKind};

performance::record_sql("SELECT * FROM users WHERE id = $1", elapsed, None, Some(1));
performance::record_http("GET", "https://api.example.com/v2/rates", Some(200), elapsed);
performance::record_span(SpanKind::View, elapsed, "receipts/show.html");

let mut timer = performance::start_span(SpanKind::Http, "POST https://api.example.com/v1/charges");
let response = client.post(url).send().await;
timer.set_status(response.status().as_u16());
timer.finish();
```

Traces group repeated SQL (same normalised text) into one span with a count; a query that ran 10 or more times in one request is flagged as an N+1 query, with the call site where it was issued (the line that called `record_sql`, or the first frame of your code when it came from sqlx).

Bounds: at most 2,000 raw spans and 1 MiB of SQL per request; 500 transaction names per minute per kind (more are counted under `(other)`); 5 unsent minutes; 30 traces per minute, each at most 300 spans and 64 KiB. A server without performance endpoints (HTTP 404) turns monitoring off for the process.

## Privacy

- **Filtered before sending:** values under sensitive keys become `"[FILTERED]"` in context, tags, breadcrumb data, request params and headers, at any depth. The default key parts mirror the Ruby gem's (and Rails') list: `passw`, `secret`, `token`, `_key`, `apikey`, `api_key`, `crypt`, `salt`, `certificate`, `otp`, `ssn`, `cvv`, `cvc`, `authorization`, `cookie`, `csrf`, `xsrf`, `sessionid`, `session_id`, `creditcard`, `credit_card`, `cardnumber`, `card_number`, `signature` and `email`. Matching ignores case and reads `-` as `_`. Add whole key names with `.filter_keys(…)`, more parts with `.filter_key_parts(…)`, or drop the defaults with `.without_default_filters()`. The user you set is sent as set (turn it off with `.send_user(false)`).
- **Never read:** request bodies, cookies and (unless you opt in) headers, query strings and request ids. URLs are sent without query strings.
- **Performance data** carries transaction names, route patterns and numbers; traces carry normalised SQL (string, number and dollar-quoted literals become `?`, comments are removed, `IN` lists and `VALUES` rows collapse), HTTP method, host and path (never credentials or query strings), and the request's method, path and request id.
- The client's own log lines (stderr, `[goodsorcery]`) name classes, counts and statuses only, and show the API key masked (`gs_live_…e81a`).
- The server scrubs again (secrets in any string, card numbers, credentials) and stores everything encrypted.

## Self-hosted

Point the client at your instance:

```sh
export GOODSORCERY_ENDPOINT=https://errors.example.com
```

or `.endpoint("https://errors.example.com")`. The endpoint may include a path prefix. It must be an `http://` or `https://` URL with a host and no `user:password@`, query string or fragment; otherwise the client stays off and logs why. Plain `http://` works but logs a warning (the key travels unencrypted) except for `localhost`.

## Delivery

Events are built and filtered in the calling thread and pushed onto a bounded queue (1,000 by default; when it is full new events are dropped and counted). The `goodsorcery-worker` thread resolves backtraces, applies `before_send` and sends batches of up to 25 events to `/api/v1/events/batch` (gzip from 1 KiB, keep-alive, rustls), when 25 are waiting or 2 seconds have passed. It retries 5xx and network errors up to 3 times with exponential backoff and jitter, waits out `429` for `Retry-After` without spending a retry, splits a batch refused as too large, falls back to one request per event on a server without the batch endpoint, and drops what the server rejects (an invalid key is logged once). Performance data goes from its own `goodsorcery-apm` thread once a minute, at a random 2–20 second offset per process.

Dropping the guard (or calling `goodsorcery::flush(timeout)`) sends what is queued, waiting up to `shutdown_timeout` (5 seconds).

## Example

[`examples/axum.rs`](examples/axum.rs) reports an error, a panic and performance data:

```sh
GOODSORCERY_API_KEY=gs_test_… cargo run --example axum
curl localhost:3001/error; curl localhost:3001/panic; curl -X POST localhost:3001/checkout
```

## Development

```sh
cargo fmt --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all-features
cargo test --no-default-features
rustup run 1.85 cargo test --all-features   # MSRV
```

The integration tests run against a fake ingest server in `tests/support`.

## License

MIT. Copyright (c) 2026 Philip Q Nguyen.

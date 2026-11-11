# Contributing

Thanks for helping. Bug reports, fixes and improvements are welcome.

- **Bugs and ideas:** open an issue at https://github.com/creativesorcery/goodsorcery-rust/issues. Include the crate version, Rust version, enabled features and, if you can, a small reproduction. Report security problems privately instead (see [SECURITY.md](SECURITY.md)).
- **Pull requests:** keep them focused, add or update tests for every behaviour change, and add a line to [CHANGELOG.md](CHANGELOG.md). For a larger change, open an issue first so we can agree on the approach.

## Development

```sh
cargo fmt --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all-features
cargo test --no-default-features
rustup run 1.85 cargo test --all-features   # MSRV
```

The integration tests (`tests/`) run against a fake ingest server in `tests/support`, so they need no network or account. CI also runs clippy for each feature on its own and checks dependencies for security advisories.

## Ground rules

These keep the crate safe to drop into any app:

- Nothing panics into the host app, and nothing blocks the caller: capture builds the event and queues it; sending happens on the client's own threads. Locks recover from poisoning, arithmetic on caller-supplied durations saturates, and the panic hook never waits on a lock its own thread might hold.
- No `unsafe` code (`#![forbid(unsafe_code)]`) and no async runtime dependency.
- Privacy first: request bodies, cookies and headers are never read unless the user opts in, sensitive keys are filtered before anything leaves the process, and the client's own log lines never contain captured data or the full API key.
- The wire format follows the ingest protocol in [goodsorcery_web's `docs/INGEST_API.md`](https://github.com/creativesorcery/goodsorcery_web/blob/main/docs/INGEST_API.md).
- MSRV is Rust 1.85 (checked in CI); raise it only deliberately, with a changelog line.

## Releasing

Maintainers release with cargo-release; the steps are in [RELEASING.md](RELEASING.md). Pull requests add their changelog line under `## Unreleased`, and the release dates that section.

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).

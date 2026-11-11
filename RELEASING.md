# Releasing

Releases are published to [crates.io](https://crates.io/crates/goodsorcery) with [cargo-release](https://github.com/crate-ci/cargo-release). Its settings are in [`release.toml`](release.toml). One command:

1. bumps the version in `Cargo.toml` and `Cargo.lock`;
2. dates the `## Unreleased` section of `CHANGELOG.md` and opens a new, empty one above it;
3. updates the version in the README's install snippets;
4. runs `cargo test --all-features`;
5. commits "Release X.Y.Z";
6. runs `cargo publish`;
7. tags the commit `vX.Y.Z`;
8. pushes `main` and the tag to `origin`.

Only maintainers who are owners of the crate on crates.io can release.

## One-time setup

1. **Install cargo-release:** `brew install cargo-release`, or `cargo install cargo-release --locked`. The newest version may need a newer Rust than you have; the error message names the last version that supports yours (`--version <that>`).
2. **crates.io account:** log in at https://crates.io with GitHub, then add and **verify your email** under Account Settings. Publishing is refused without a verified email.
3. **API token:** Account Settings › API Tokens › New Token, with the scopes `publish-new` and `publish-update`, restricted to the crate `goodsorcery`. Copy it; it is shown once.
4. **Log Cargo in:** run `cargo login` and paste the token. It is stored in `~/.cargo/credentials.toml`.

## Before each release

- Every user-visible change since the last release has a line under `## Unreleased` in `CHANGELOG.md`. Contributors add these in their pull requests.
- You are on `main`, up to date with `origin/main`, with a clean working tree. cargo-release refuses otherwise.
- CI is green on `main`.

## Choosing the version

The crate follows [semantic versioning](https://semver.org). Before 1.0, Cargo treats the middle number as the breaking one:

| Level   | Example         | Use for                                                       |
|---------|-----------------|---------------------------------------------------------------|
| `patch` | 0.1.0 → 0.1.1   | Bug fixes only.                                               |
| `minor` | 0.1.1 → 0.2.0   | New features, or any breaking change while the crate is 0.x.  |
| `major` | 0.2.0 → 1.0.0   | Declaring the API stable, or a breaking change after 1.0.     |

Raising the minimum supported Rust version (`rust-version` in `Cargo.toml`) counts as a `minor` release and needs a changelog line.

## Release

From the crate's directory:

```sh
cargo release patch              # dry run: prints every step, changes nothing
cargo release patch --execute    # the real release
```

Replace `patch` with `minor` or `major` as needed. Read the dry run's output before running it with `--execute`: it shows the CHANGELOG and README edits, runs the tests, builds the package and stops at "aborting upload due to dry run". Warnings like "ignoring test `axum` as `tests/axum.rs` is not included in the published package" are expected; the integration tests stay in the repository and out of the package.

**First release only:** `Cargo.toml` already says `0.1.0`, which has never been published, so release it without a level:

```sh
cargo release              # dry run
cargo release --execute
```

## Afterwards

- https://crates.io/crates/goodsorcery shows the new version within a minute.
- docs.rs builds the API documentation automatically, in a few minutes: https://docs.rs/goodsorcery.
- Optionally, create a GitHub release from the tag (Releases › Draft a new release › choose the tag) and paste the changelog section.

## If something goes wrong

- **The tests or the dry run fail:** nothing has been committed or published. Fix the problem on `main` and start again.
- **`cargo publish` fails** (network, crates.io down, a packaging error): the release commit exists locally but is not tagged or pushed. Undo it with `git reset --hard HEAD~1`, fix the problem, and run the release again.
- **Publishing succeeded but the push failed:** finish by hand with `git push origin main` and `git push origin vX.Y.Z`.
- **A published version is broken:** a version can never be deleted or overwritten. Release a fixed patch version, then yank the broken one with `cargo yank --version X.Y.Z`. Yanking stops new projects from picking that version; existing lock files keep working. `cargo yank --version X.Y.Z --undo` reverses it.

## Crate owners

Keep at least two owners, so a release never depends on one person's account:

```sh
cargo owner --list
cargo owner --add <github-username>                  # a person (they need a crates.io account)
cargo owner --add github:creativesorcery:<team>      # a GitHub team in the organization
```

A GitHub team needs the crates.io application to have access to the organization first (an organization owner grants it under GitHub › Settings › Applications). Team owners can publish but can't add or remove other owners.

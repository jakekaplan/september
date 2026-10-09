# Contributing to September

September has a deterministic memory core and a local HTTP service with
SQLite storage, in memory or in a file. Shared Postgres storage and authentication remain
future work. Add dependencies only for implemented behavior.

## Setup

Install [rustup](https://rustup.rs/) and [uv](https://docs.astral.sh/uv/).
`rust-toolchain.toml` selects Rust 1.99, rustfmt, and Clippy. Install
[Just](https://github.com/casey/just) if you want recipe shortcuts; all commands
can also run directly.

All commands below run from the repository root. No database server, model
credentials, or attachment storage are needed to test either crate; SQLite is
bundled, and its tests use temporary files.

## Focused local checks

```sh
cargo fmt --all -- --check
cargo clippy --locked -p september-memory --all-targets --all-features -- -D warnings
cargo test --locked -p september-memory <test-name-filter>
uv tool run --from 'loq>=0.1.0' loq check <changed-paths>
```

Use `-p september` for service changes. Choose the narrowest relevant test;
run the affected package's tests when a filter is not useful. Workspace tests
belong in CI unless explicitly requested locally. The memory crate has unit,
cross-module interaction, and documentation tests using completed summaries
and built-parent lookups. `cargo test --locked -p september-memory` runs
this affected package's tests; these establish cover and navigation invariants,
not archive publication guarantees, service behavior, or summary quality.
`cargo test --locked -p september` covers concurrent ingestion, retry conflicts,
pending snapshot cutoffs, immutable retrieval, lease fencing, bounds, and HTTP
handlers without opening a socket. Summary outputs are deterministic test data.

With Just:

```sh
just check september-memory
just test september-memory <test-name-filter>
```

`cargo fmt --all` or `just fmt` applies formatting. `just ci` is an explicit,
exhaustive command, not a prerequisite for every local task.

## Optional commit hooks

```sh
uv tool run --from 'prek>=0.5.5' prek install
uv tool run --from 'prek>=0.5.5' prek run --files <changed-paths>
```

Or use `just hooks-install` and `just hooks <paths...>`. Hooks check Rust
formatting and file sizes. They do not run tests, make model calls, or duplicate
all CI checks. Hook installation is opt-in per checkout.

## Code and tests

- Keep pure memory rules in `crates/memory`; I/O belongs in `crates/service`.
  Restore only the saved cover's completed summaries, not a full-history tree.
  Supply all eligible ready parents for compaction. Zoom plans range lookups;
  the service retrieves content and enforces immutable publication and child
  readiness. Do not introduce a generic storage framework into the core.
- Use narrow visibility and domain names. Split files by responsibility and keep
  them under 500 lines. See `AGENTS.md` for the complete conventions.
- Add focused behavior tests with features and regression tests with fixes.
- Keep unit tests with their owner; put cross-boundary tests in the affected
  crate's `tests/` directory. Do not create catch-all test modules.
- Prefer in-process HTTP tests and isolated real-Postgres integration tests when
  those boundaries exist. Use fake model responses for automated tests.
- Assert rendered views and memory invariants explicitly. Golden-file tests may
  be useful for future CLI output; generate those files through tests and review
  their diffs rather than substituting them for correctness assertions.

## Dependencies

Check the registry for the latest stable release before adding a package. Use
`>=` requirements, choose features explicitly, and commit `Cargo.lock`. Add a
workspace dependency only when more than one crate uses it. Both internal crates
are intentionally non-publishable.

Install the current dependency checker with Cargo if it is not available:

```sh
cargo install --locked cargo-deny --version '>=0.20.2'
cargo deny --locked check
```

`cargo-deny` checks advisories, licenses, version duplication, and dependency
sources. It is sufficient for the current policy; there is no parallel
`cargo-audit` requirement. Investigate findings rather than adding blanket
exceptions. Rust baseline changes must update the toolchain, Cargo metadata,
and Clippy MSRV together.

## CI and review

GitHub Actions runs the full workspace checks on Linux, the initial hosted
service target. It has read-only repository permissions, pinned action commits,
Rust caching, and cancellation of superseded runs. There is no coverage gate,
Codecov upload, packaging workflow, or deployment workflow.

Before handing off a change, describe what changed and list the focused checks
actually run, including relevant checks that could not run. Keep credentials
and conversation contents out of public logs and examples. Read `SECURITY.md`
when touching ingestion, retrieval, persistence, or model calls.

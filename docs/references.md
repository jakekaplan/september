# Rust reference projects

Use each project as a reference for a specific responsibility, not as a template
for September's overall size. September starts with two crates, no runtime
dependencies, and no application implementation.

The conventions were reviewed on 2026-10-08. Links below pin inspected revisions
so the evidence remains useful when upstream guidance changes. Recheck upstream
APIs and releases when implementation begins; these are not dependency pins.

## Ruff: engineering north star

Repository: [astral-sh/ruff](https://github.com/astral-sh/ruff)

Reviewed revision: `002095b37dc7e4f05c6f06cf5d440d151b1c2bad`.

- [Contributing guide](https://github.com/astral-sh/ruff/blob/002095b37dc7e4f05c6f06cf5d440d151b1c2bad/CONTRIBUTING.md):
  flat crate layout, ownership by domain, and explicit rules for unpublished
  internal crates.
- [Agent guidance](https://github.com/astral-sh/ruff/blob/002095b37dc7e4f05c6f06cf5d440d151b1c2bad/AGENTS.md):
  narrow visibility, focused tests, justified lint expectations, and writing
  documentation for readers who do not know the development conversation.
- [Workspace manifest](https://github.com/astral-sh/ruff/blob/002095b37dc7e4f05c6f06cf5d440d151b1c2bad/Cargo.toml):
  centralized metadata, lint policy, and deliberate build profiles.

Borrow engineering discipline and readable diagnostics. Do not copy its many
crates, Python packaging, compiler-specific machinery, or automatic snapshot
acceptance workflow. September reviews generated snapshot changes explicitly.

## Vector: production service behavior

Repository: [vectordotdev/vector](https://github.com/vectordotdev/vector)

Reviewed revision: `3a8a5d4b73a7a02d762f67c7081f3e0ed561e8a5`.

- [Architecture](https://github.com/vectordotdev/vector/blob/3a8a5d4b73a7a02d762f67c7081f3e0ed561e8a5/docs/ARCHITECTURE.md):
  bounded in-flight work, ordered completion, backpressure, buffering, and
  explicit lifecycle ownership.
- [Rust style](https://github.com/vectordotdev/vector/blob/3a8a5d4b73a7a02d762f67c7081f3e0ed561e8a5/docs/RUST_STYLE.md):
  structured tracing, tests beside their owner, and deliberate dependency feature
  placement rather than accidental feature unification.
- [Instrumentation](https://github.com/vectordotdev/vector/blob/3a8a5d4b73a7a02d762f67c7081f3e0ed561e8a5/docs/specs/instrumentation.md):
  first-class telemetry, bounded metric labels, and distinguishing retriable
  failures from data actually dropped.
- [Agent guidance](https://github.com/vectordotdev/vector/blob/3a8a5d4b73a7a02d762f67c7081f3e0ed561e8a5/AGENTS.md):
  run the narrowest relevant tests with the minimum necessary feature set.

Use these principles for ingestion, summary jobs, shutdown, and reporting.
Do not introduce Vector's dynamic topology, component framework, or enormous
feature matrix into a single-purpose memory service.

## Axum: HTTP boundary and testability

Repository: [tokio-rs/axum](https://github.com/tokio-rs/axum)

Reviewed revision: `618496288c41da835683ec149432c4869722779f`.

- [Testing example](https://github.com/tokio-rs/axum/blob/618496288c41da835683ec149432c4869722779f/examples/testing/src/main.rs):
  construct a router separately from the listener and test it as a Tower service
  without binding a socket. Reserve real networking tests for transport behavior.
- [Graceful shutdown example](https://github.com/tokio-rs/axum/blob/618496288c41da835683ec149432c4869722779f/examples/graceful-shutdown/src/main.rs):
  handle termination and bound outstanding requests so shutdown cannot hang.
- [Postgres example](https://github.com/tokio-rs/axum/blob/618496288c41da835683ec149432c4869722779f/examples/sqlx-postgres/src/main.rs):
  application state with a connection pool and database errors translated at the
  HTTP boundary.

These are instructional examples, not production policy. In particular, do not
copy example `unwrap` calls, credentials, or generic error disclosure into
September's production paths.

## SQLx: database correctness

Repository: [launchbadge/sqlx](https://github.com/launchbadge/sqlx)

Reviewed revision: `8b65c2fb42a3a523b77000f41b648986d1d49ba0`.

- [`sqlx::test` documentation](https://github.com/launchbadge/sqlx/blob/8b65c2fb42a3a523b77000f41b648986d1d49ba0/src/macros/test.md):
  isolated real databases per test, automatic migrations, fixtures, and the
  connection-limit implications of parallel tests.
- [Axum application with tests](https://github.com/launchbadge/sqlx/tree/8b65c2fb42a3a523b77000f41b648986d1d49ba0/examples/postgres/axum-social-with-tests):
  an example of HTTP behavior tested against database state.

Use real Postgres tests for transactions, deduplication, durable snapshots, and
job claims. Mocks do not establish database locking or crash-recovery semantics.
Use minimal features and do not copy SQLx's multi-database support matrix.

## Official Rust MCP SDK: protocol correctness

Repository: [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)

Reviewed revision: `08e021153ef0530aeb0bb406ebb360a38cfb8ee4`.

- [Streamable HTTP server](https://github.com/modelcontextprotocol/rust-sdk/blob/08e021153ef0530aeb0bb406ebb360a38cfb8ee4/examples/servers/src/counter_streamhttp.rs):
  mount MCP inside Axum and connect transport cancellation to process shutdown.
- [Authentication example](https://github.com/modelcontextprotocol/rust-sdk/blob/08e021153ef0530aeb0bb406ebb360a38cfb8ee4/examples/servers/src/simple_auth_streamhttp.rs):
  inspect the authentication boundary when choosing an implementation; example
  credentials and test authorization flows are not deployment defaults.
- [SDK documentation](https://github.com/modelcontextprotocol/rust-sdk/blob/08e021153ef0530aeb0bb406ebb360a38cfb8ee4/README.md):
  tools, resources, structured results, error semantics, and protocol negotiation.

Keep MCP as a transport adapter over the same service operations as HTTP.
Do not hand-build JSON-RPC or move archive ownership into MCP session state.
Protocol features and client compatibility must be checked at implementation time.

## Local foundation: loq

The sibling `../loq` repository supplied the initial conventions:

- An agent guide with a crate ownership map and a `CLAUDE.md` symlink.
- Focused crates, workspace metadata/lints, unsafe-code prohibition, typed errors,
  a 500-line limit, and straightforward formatting and dependency checks.
- Simple Just recipes and optional local hooks.

Adaptations for September:

- Rust 1.99, edition 2024, resolver 3, and one consistent Clippy MSRV instead of
  loq's moving stable toolchain and stale 1.75 MSRV.
- Focused local checks, with exhaustive workspace validation in CI.
- No copied coverage quota or Codecov integration, Python packaging, wheel
  workflows, benchmark scripts, or premature release automation.
- One optional hook mechanism. Do not duplicate it with a second `.githooks`
  directory or force developers to install Just.
- Non-publishable crates and an explicit, still-unselected project license.

## Memory-design sources

- [UniiChat design](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449):
  automatic conversation capture, immutable summary nodes, persisted batched
  views, background compression, and zoom retrieval. The reviewed gist revision
  is `4c09901baa3685852bf252cdb70ace81d01d6be5`.
- [OptMem](https://github.com/VictorTaelin/OptMem/tree/1fb164cf39028047781f72ac3bb1e5a691c1dcb0):
  the earlier agent-selected-note implementation. Its repository had no explicit
  license at review time; use it as conceptual reference, not copied code.

September adapts the gist's single-process storage into a hosted,
transactional service, with provenance and per-interaction snapshot isolation
across concurrent clients. It does not claim the gist's cache simulations are
measured production results, or that retaining originals makes lossy summaries
perfectly searchable.

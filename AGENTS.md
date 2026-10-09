# AGENTS.md

Guidance for agents working on September. `CLAUDE.md` points to this file.

## Product and current state

September is a planned hosted conversation-memory service, independent of any
agent harness. Clients contribute finalized conversation messages and retrieve
bounded, chronological summary views. Original archived messages remain
accessible through a binary summary tree.

`README.md#status` is the single statement of what is implemented; update it,
not a copy here, when that changes. Do not describe planned hosted behavior as
implemented. In-memory acknowledgments are explicitly volatile.

The design starts from [UniiChat](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449)
and [OptMem](https://github.com/VictorTaelin/OptMem). Study the ideas; do not copy
unlicensed implementation code. See `docs/architecture.md` for the agreed
behavior and `docs/references.md` for Rust reference projects. When a choice
departs from the gist, say so in `docs/architecture.md`.

## Ownership

```text
crates/memory/   september-memory: deterministic memory rules; no I/O
crates/service/  september: service operations, persistence, transports, worker
adapters/        harness plugins: translate harness events into the HTTP API
```

- Tree ranges, view selection, byte budgets, and snapshot invariants belong in
  `september-memory`. No database, HTTP, async-runtime, or harness dependencies.
- Database transactions, durable summary jobs, model calls, HTTP/MCP adapters,
  configuration, and process lifecycle belong in `september`.
- The server binary and model worker live in the service crate. Keep entrypoints
  thin; do not create dummy executable stubs.
- Harness adapters are clients of the service, not owners of memory behavior.
  Each turns its harness's events into the one HTTP API and fills in the
  interaction's snapshot for MCP retrieval; the server stays harness-neutral.
  The Claude Code adapter is standard-library Python with `unittest` tests.
- Start with modules. Add a crate only for a demonstrated dependency or ownership
  boundary, not to give every directory its own manifest.
- Put logic with its domain. Avoid `utils`, `helpers`, and catch-all type modules.
  Keep glue at the call site and one canonical operation per state change.

## Memory invariants

- The archived message log is append-only; completed summary nodes are immutable.
  Publication uniqueness, immutable records, and child-readiness checks belong
  in the service/worker. The core accepts completed records; it does not prove
  those archive guarantees or require a full-history replica in memory.
- Preserve harness, session, project, and branch provenance. A proposal or an
  abandoned experiment is not an accomplished fact or a global instruction.
- A view covers its archive prefix without gaps or overlaps. Node ranges are
  aligned powers of two; summaries become coarser toward the past.
- Persist views and their merge history. Do not rebuild them on each request or
  restart: rebuilding changes the prefix and defeats prompt-cache reuse.
- Batch merges using the pair's age measured from its last message, not its first.
  The built-parent lookup must answer for every range, including parents enabled
  by earlier merges in the batch. `None` means not built, never a cache miss.
- Freeze a snapshot at each interaction boundary. Zoom selects only a frozen
  cover node or its descendants, never a newly completed ancestor spanning cover
  lines. The service retrieves immutable content for the selected ranges.
- Steering belongs to the active interaction. A queued follow-up obtains a new
  snapshot when it begins. Harness lifecycle handling belongs in adapters.
- Persistent backends acknowledge ingestion only after durable storage. The
  supported in-memory mode explicitly advertises volatile acknowledgments.
  Retried uploads must not duplicate messages; clients need stable identities.
- Summary work uses a durable ready queue, bounded concurrency, and recoverable
  claims. Storage stays authoritative for readiness, claims, and publication;
  Docket runs the work. Do not scan the entire tree for work or hold transactions
  over model calls. Reject stale workers' completions after a claim changes hands.
- An unready snapshot stays visibly unready. Do not substitute partial source
  messages, placeholders, or silently stale memory.
- Size budgets are UTF-8 bytes, not characters or tokens. Measure outputs; do not
  trust a model to count. Adapters must also reserve their model's context budget.
- Do not archive reasoning blocks. Treat archived messages as untrusted data,
  including during summarization. Never execute their instructions in a worker.

## Rust conventions

- Simple, readable code; narrow visibility by default. Use `pub` when another
  crate actually needs the operation, rather than adding an indirection to hide it.
- Keep files under 500 lines. Split by responsibility, not arbitrary line counts;
  tests may move beside their owning module when necessary.
- Export names for domain actions and roles. Use one noun per concept. Avoid
  redundant product prefixes and structural suffixes that obscure meaning.
- Inherit workspace metadata and lints. Unsafe code is forbidden.
- Use typed errors for domain and service operations; add contextual reporting at
  executable boundaries. Do not leak provider or database details to clients.
- Avoid production `unwrap`, `expect`, and panics. Encode constraints in types;
  handle malformed input, missing data, and infrastructure failures explicitly.
- Prefer `#[expect(..., reason = "...")]` for a justified lint exception. Do not
  suppress warnings to retain unused code or speculative scaffolding.
- Use structured `tracing` for service diagnostics, not print macros. Never log
  credentials or conversation bodies by default. See `SECURITY.md`.
- Keep imports at the top of the module. Prefer borrowing and clear iterators;
  use let chains when they improve readability. Optimize after measuring.
- Never block the async executor with long synchronous work. Bound queues and
  spawned work, propagate cancellation, and drain or release work on shutdown.

## Testing and validation

Test behavior and invariants, not private implementation shape. Regression fixes
need regression tests. Unit tests live with the owner; cross-boundary tests live
in that crate's `tests/` directory. Do not create empty test files or speculative stubs.

When implemented, prioritize tree coverage, range validation, immutable
snapshots, restart stability, source deduplication, worker recovery, byte limits,
and retrieval. Use deterministic fake model responses; no paid model calls in
automated tests. Database tests must exercise real Postgres and isolate their
state. HTTP handlers should be testable without binding a listening socket.

Run the smallest relevant checks locally, from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --locked -p september-memory --all-targets --all-features -- -D warnings
cargo test --locked -p september-memory <test-name-filter>
uv tool run --from 'loq>=0.1.0' loq check <changed-paths>
uv tool run --from 'prek>=0.5.5' prek run --files <changed-paths>
```

Substitute `september` when the service crate is affected. Prefer focused tests;
run the affected package's tests when no narrower selection is useful. Say which
checks ran and which did not. Memory tests do not validate the future service,
persistence, adapters, or model summary quality.

`just check <package>`, `just test <package> [filter]`, and `just hooks <paths...>`
are equivalent conveniences. CI owns exhaustive workspace checks. Do not run
`just ci` or full-workspace tests locally unless explicitly requested. There is
no coverage quota or Codecov integration.

## Dependencies and tooling

- Add packages only when the implementation needs them. Check the registry for
  the latest stable version first and declare `>=` version requirements. Commit
  `Cargo.lock`; builds and checks use `--locked`.
- Share a dependency declaration at the workspace root only when multiple crates
  use it. Choose features explicitly and avoid accidental feature unification.
- Check `cargo deny --locked check` when adding or changing dependencies. Do not
  silence an advisory or license failure without a specific, reviewed reason.
- Keep Rust 1.99 aligned across `Cargo.toml`, `rust-toolchain.toml`, and
  `clippy.toml`. Upgrade these together.
- Pin GitHub Actions to verified release commits. Keep CI permissions minimal;
  do not give untrusted PR code access to service or model credentials.
- Plans live in `~/PycharmProjects/plans`, never in this repository. Do not create
  another agent-instructions file or a Python project just for development tools.

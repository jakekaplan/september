# September

A planned hosted conversation-memory service for AI agents, independent of their
harness. Pi, Claude, custom agents, and command-line clients can contribute to one
archive and retrieve older details through a summary tree.

**Status: memory core and a local HTTP service with in-memory storage.**
`september-memory` has checked tree ranges, owned summary covers, batched views,
saved-view restoration, and frozen zoom range selection. The service supports
deduplicated ingestion, immutable snapshots, zoom to originals, and recoverable
summary claims through an interchangeable storage contract. In-memory storage is
the first backend: restarting loses all data. An opt-in worker summarizes with
OpenAI or Anthropic. No persistent database, MCP endpoint, or harness adapter is
implemented yet.

## Run locally

```sh
cargo run --locked -p september
```

The server listens on `127.0.0.1:3000`. `SEPTEMBER_STORAGE=memory` is the default
and currently the only backend. `SEPTEMBER_BIND` can select another loopback
address/port. Non-loopback binds are rejected until authentication is implemented.

See the [HTTP walkthrough](docs/http.md) to upload a message, freeze an interaction,
and retrieve its original text. Short records enter memory verbatim; longer
records and parent summaries wait for explicit worker completion by default.
The walkthrough includes the claim/completion API.

## Continuous worker

Choose a provider and model, with its standalone API key already set in your
environment:

```sh
SEPTEMBER_SUMMARIZER=openai SEPTEMBER_MODEL=gpt-6-luna cargo run --locked -p september
# Or select anthropic, set SEPTEMBER_MODEL to your chosen Claude model,
# and supply ANTHROPIC_API_KEY instead of OPENAI_API_KEY.
```

Settings use `config` + Serde: defaults, then an optional TOML file selected by
`SEPTEMBER_CONFIG`, then environment overrides. An explicitly selected missing
file, unknown settings, invalid provider, absent model/key, or non-loopback bind
fails startup. API keys are read only from `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`;
no harness login or OAuth state is used. `.env` files are not loaded automatically.

For example, `SEPTEMBER_CONFIG=/path/to/september.toml` loads:

```toml
storage = "memory"
bind = "127.0.0.1:3000"
summarizer = "openai"
model = "gpt-6-luna"
```

`genai` handles OpenAI Responses and Anthropic Messages inference. The model
receives the immutable input and frozen historical context as untrusted data.
Parent children whose combined text plus separator fits within 512 bytes are
joined without a model call. Otherwise September measures the final UTF-8 text
and allows five corrections after the initial response. Oversized, empty,
truncated, refused, or non-text output fails the attempt; it is never clipped
or published as partial memory. Reasoning is excluded. OpenAI requests set
`store=false`. Each request has a 90-second timeout and a 2,048-output-token cap,
within the worker's overall two-minute attempt deadline.

Enable the fake summarizer only for synthetic local data:

```sh
SEPTEMBER_SUMMARIZER=fake cargo run --locked -p september
```

The default is `SEPTEMBER_SUMMARIZER=none`, which leaves summary jobs available
for manual completion. Fake mode labels summaries `FAKE` and adds
`x-september-summarizer: fake` to HTTP responses. It does not preserve message
meaning and must not be used for real conversations.

The continuous worker uses Docket's `memory://` queue, dispatches at most eight
jobs, and renews September's 60-second claims every 20 seconds while jobs run or
wait for retry. Each attempt has a two-minute timeout. Three failed attempts
release the claim with a 30-second backoff; the snapshot stays pending. Shutdown
stops new claims, drains for ten seconds, then cancels unfinished work and
releases its claims. Cleanup has a further bounded deadline.

Each claim includes historical context from a retained smaller view with a
32,000-byte trigger and 16,000-byte target. Leaf context excludes the source
message; parent context includes history through its two children. Both stop at
the first unbuilt leaf. Context freezes on the first claim and survives retries.
Jobs wait if their context exceeds 32,000 bytes while other ready parents run.

Run the isolated synthetic example or worker tests:

```sh
cargo run --locked -p september --example docket
cargo test --locked -p september --test worker
```

The example uses the same continuous worker as the server. Eight
synthetic messages produce eight leaf summaries and seven parent summaries,
turning a pending snapshot ready. It shares no data with the HTTP server and
makes no model calls. Tests cover incoming work, bounded concurrency, retries,
claim renewal, backoff, graceful and forced shutdown, interruption recovery,
stale completion, byte limits, and frozen retrieval.

September remains authoritative for ready jobs and immutable publication. A
claim lost before enqueue recovers through expiry. Both stores are volatile:
whole-process restart durability and Redis deployment remain unimplemented.
Live summary quality has not been validated. Provider spend limits and persistent
storage remain future work; repeated failed jobs continue retrying after backoff.

## Intended behavior

Inspired by [UniiChat](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449)
and [OptMem](https://github.com/VictorTaelin/OptMem):

- Automatically archive conversation messages, tool calls, and results supplied
  by adapters, excluding reasoning blocks.
- Summarize the archive into an immutable binary tree in the background.
- Persist a bounded chronological view: older history is coarse, recent history
  is detailed. Batch merges rather than continuously rewriting the prefix.
- Freeze the view for each interaction. Other sessions can keep contributing
  without changing that interaction's memory or retrieval results.
- Expose HTTP for lifecycle integration and MCP for retrieval. A future CLI can
  call the same HTTP API; the service does not depend on MCP support in a harness.

The service owns memory and summarization. Adapters own capture, interaction
boundaries, and constructing model context. Adding an MCP server alone cannot
make a harness replace its conversation history.

## Repository

```text
crates/memory/   september-memory: deterministic memory rules, no I/O
crates/service/  september: HTTP server, atomic storage, in-memory backend
```

Start with two crates. Keep transport, persistence, and worker responsibilities
in service modules until a real dependency boundary warrants another crate.

## Memory core

The core holds only the summaries in the current view, not the full archive.
`Summary` binds immutable text to a checked node range. `View` appends completed
leaves and batch-merges using supplied ready-parent text, counting actual rendered
UTF-8 bytes. An unfinished batch retains its shrinking state across restoration.

`Snapshot` fixes the view and allows zoom only into one of its lines or that
line's descendants. Zoom returns child ranges or an original-message ID; the
service retrieves their contents. A later-completed ancestor cannot
expand an active interaction's allowed navigation.

Run its focused unit, interaction acceptance, and documentation tests:

```sh
cargo test --locked -p september-memory
```

There is no persistence format yet. Save the view's node identities and shrinking
state together, then load only the referenced completed summaries with the same
budget. Restoring one line covering 1,024 messages needs that one summary, not
its descendants. Compaction takes a read-only map of eligible ready parents;
absence means not ready, not an unchecked cache miss.

The service enforces unique, immutable publication and completed children.
`cargo test --locked -p september` covers storage and HTTP behavior with supplied
summaries. Memory tests establish structural behavior, not service guarantees
or model summary quality.

## Development

Install [rustup](https://rustup.rs/) and [uv](https://docs.astral.sh/uv/). The
repository selects Rust 1.99 with Clippy and rustfmt. No Python project or global
Python tool installation is required.

From the repository root:

```sh
cargo metadata --no-deps --locked --format-version 1
cargo fmt --all -- --check
cargo clippy --locked -p september-memory --all-targets --all-features -- -D warnings
cargo clippy --locked -p september --all-targets --all-features -- -D warnings
uv tool run --from 'loq>=0.1.0' loq check .
```

[Just](https://github.com/casey/just) is optional. `just` lists recipes;
`just check september-memory` checks an affected package. See
[CONTRIBUTING.md](CONTRIBUTING.md) for tests, hooks, and dependency checks.

## Design and reference projects

- [Architecture](docs/architecture.md): service boundaries, memory rules, and
  decisions still to make.
- [Rust references](docs/references.md): what to learn from Ruff, Vector, Axum,
  SQLx, and the official MCP SDK, with inspected revisions.
- [Agent guidance](AGENTS.md): ownership, coding conventions, and validation.
- [Security policy](SECURITY.md): trust boundaries and sensitive-data handling.

CI checks formatting, Clippy, file sizes, workspace tests, and dependency policy.
There is no Codecov integration or coverage quota. No project license has been
selected; both internal crates are marked non-publishable.

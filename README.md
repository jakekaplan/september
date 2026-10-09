# September

A planned hosted conversation-memory service for AI agents, independent of their
harness. Pi, Claude, custom agents, and command-line clients can contribute to one
archive and retrieve older details through a summary tree.

The design follows [UniiChat](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449)
and [OptMem](https://github.com/VictorTaelin/OptMem): every message is archived
word for word, a background model compresses the archive into a binary tree of
512-byte summaries, and each interaction sees a bounded, chronological view in
which recent lines are fine and old lines coarse. Any line can be zoomed back
down to the original message.

## Status

This README is the single statement of what is implemented.

- **Memory core** (`september-memory`): aligned ranges, batched views with the
  gist's merge order and 64–128 KB sawtooth, saved-view restoration, and frozen
  snapshots that zoom only into their own cover.
- **Local service** (`september`): deduplicated ingestion, fixed-cutoff
  snapshots, zoom to originals, fenced summary claims, and a retained 16–32 KB
  compaction view for worker context, all behind an interchangeable `Storage`
  contract. Short messages and short pairs of summaries publish verbatim, with
  no model call.
- **Storage**: in memory only. Acknowledgments are volatile; restarting loses
  all data.
- **Worker**: opt-in Docket worker on an in-process `memory://` queue, with
  OpenAI, Anthropic, or visibly fake summaries.
- **Not implemented**: persistent storage, Redis-backed Docket, authentication,
  MCP, a CLI, and harness adapters. Live summary quality is unvalidated.

## Run locally

```sh
cargo run --locked -p september
```

The server listens on `127.0.0.1:3000`. Non-loopback binds are rejected until
authentication exists. The [HTTP walkthrough](docs/http.md) uploads a message,
freezes an interaction, and zooms to the original.

By default no summarizer runs, and long messages wait for an external worker to
claim and complete them over HTTP. To summarize continuously, choose a provider
and model and set that provider's standalone API key:

```sh
SEPTEMBER_SUMMARIZER=openai SEPTEMBER_MODEL=gpt-6-luna cargo run --locked -p september
# Or SEPTEMBER_SUMMARIZER=anthropic with a Claude model and ANTHROPIC_API_KEY.
```

For synthetic data only, `SEPTEMBER_SUMMARIZER=fake` writes summaries labelled
`FAKE` and marks every response `x-september-summarizer: fake`.

Settings come from defaults, then an optional TOML file named by
`SEPTEMBER_CONFIG`, then `SEPTEMBER_*` environment variables. Unknown settings,
a missing model or key, or a missing explicit file fail startup. API keys are
read only from `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`; harness logins are never
used, and `.env` files are not loaded.

```toml
bind = "127.0.0.1:3000"
summarizer = "openai"
model = "gpt-6-luna"
```

## How summaries are built

A message that fits in 512 bytes as a tagged line, such as `user: ...`, is its
own summary. Two children that fit in 512 bytes together are joined by a
newline. Only the rest become jobs.

The worker uses the gist's compaction call. The job's frozen `<chat>` context
comes first. Then comes the task, with a 512-dash ruler showing the size. A
draft over the limit gets the gist's "Too long" reply, which shows where the
limit cuts it. After five attempts the shortest draft is kept: the view measures
real sizes. Storage accepts summaries up to 1,024 bytes, and the summarizer cuts
a draft at that ceiling if it is still longer. One stubborn summary can
therefore never stall memory.

Docket runs the background work. A perpetual dispatch task claims ready jobs
from storage, which caps active claims at eight. It adds one summary task per
claim. Each task renews its 60-second claim every 20 seconds while the model
runs, and Docket retries a failed attempt three times. If every attempt fails,
the claim lapses and the job becomes claimable again. Storage stays
authoritative for readiness, fencing, and publication.

## Repository

```text
crates/memory/   september-memory: deterministic memory rules, no I/O
crates/service/  september: HTTP server, storage, summarizer, Docket worker
```

Install [rustup](https://rustup.rs/) and [uv](https://docs.astral.sh/uv/); the
repository selects Rust 1.99 with Clippy and rustfmt. Focused checks, hooks, and
dependency policy are in [CONTRIBUTING.md](CONTRIBUTING.md).

- [Architecture](docs/architecture.md): the hosted design, memory rules, and open
  decisions.
- [HTTP contract](docs/http.md): routes, schemas, and limits.
- [Rust references](docs/references.md): what to learn from Ruff, Vector, Axum,
  SQLx, and the official MCP SDK.
- [Agent guidance](AGENTS.md) and [security policy](SECURITY.md).

CI checks formatting, Clippy, file sizes, tests, rustdoc, and dependency policy.
There is no coverage quota. No license has been selected; both crates are
unpublished.

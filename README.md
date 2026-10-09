# September

[![CI](https://github.com/jakekaplan/september/actions/workflows/ci.yml/badge.svg)](https://github.com/jakekaplan/september/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

🎶 *Do you remember... the 21st night...* 🎶

Long-term memory for AI agents. September keeps every message your agents
exchange, compresses the history into a small view that fits in context, and
lets the agent zoom back into any part of it, down to the original words. One
memory is shared by every harness and session.

It follows Victor Taelin's [UniiChat](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449)
and [OptMem](https://github.com/VictorTaelin/OptMem) design.

## How it works

- Every message is archived word for word.
- A model summarizes the archive into a binary tree of one-line summaries, each
  at most 512 bytes. Two neighbouring lines merge into one as they age.
- Each session starts with a view of the whole history: recent messages one per
  line, older work in fewer, coarser lines.
- The agent calls `zoom` to open any line into the two lines it was made from,
  and on down to the original message.

## Quick start

```sh
cargo run --locked -p september
```

The server listens on `127.0.0.1:3000` and keeps everything in memory. To keep
memory across restarts and summarize long messages, give it a database file and
a model:

```sh
export ANTHROPIC_API_KEY=...
SEPTEMBER_STORAGE=sqlite://data/september.sqlite3 \
SEPTEMBER_SUMMARIZER=anthropic SEPTEMBER_MODEL=claude-haiku-5-5 \
cargo run --locked -p september
# Or SEPTEMBER_SUMMARIZER=openai SEPTEMBER_MODEL=gpt-6-luna with OPENAI_API_KEY.
```

Without a summarizer, any message over 512 bytes stays unsummarized and the
view waits for it. The [HTTP walkthrough](docs/http.md) shows the API by hand.

## Use with Claude Code

With the server running, install the plugin once:

```sh
claude plugin marketplace add ./adapters
claude plugin install september@september
```

Then use `claude` as usual. The plugin archives each prompt, tool call, tool
result and everything Claude writes, loads the memory view when a session starts (and after
`/clear` or compaction), and gives Claude `zoom` and `date` tools. Set
`SEPTEMBER_URL` if the server is not at `http://127.0.0.1:3000`.

Current limits: uploads are best effort, so messages sent while the server is
down are lost; subagent work is not captured yet.

## Settings

Defaults, then an optional TOML file named by `SEPTEMBER_CONFIG`, then
environment variables:

| Setting      | Variable               | Default              | Values                                 |
| ------------ | ---------------------- | -------------------- | -------------------------------------- |
| `bind`       | `SEPTEMBER_BIND`       | `127.0.0.1:3000`     | a loopback address                     |
| `storage`    | `SEPTEMBER_STORAGE`    | `memory`             | `memory` or `sqlite://<path>`          |
| `summarizer` | `SEPTEMBER_SUMMARIZER` | `none`               | `none`, `anthropic` or `openai`        |
| `model`      | `SEPTEMBER_MODEL`      |                      | required with a summarizer             |
| `queue`      | `SEPTEMBER_QUEUE`      | `memory://september` | in process, or a `redis://` URL        |

API keys come only from `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`.

## Status

This section is the single statement of what is implemented.

- **Works:** archiving, the summary tree and views, snapshots, and zoom, over
  HTTP and MCP; SQLite storage in memory or in a file; background summaries at
  xhigh reasoning effort, as in the gist, with Anthropic (tested live with Haiku)
  or OpenAI (untested live); the Claude Code plugin.
- **Untested:** a Redis queue.
- **Not built:** authentication and hosting, Postgres, a Pi adapter, a CLI.
- **Open:** summary quality is only spot-checked so far.

## Development

```text
crates/memory/   september-memory: the memory rules, no I/O
crates/service/  september: server, storage, summarizer, worker
adapters/        harness plugins; claude-code is standard-library Python
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and checks,
[architecture](docs/architecture.md) for the design and how summaries are built,
the [HTTP contract](docs/http.md), [AGENTS.md](AGENTS.md), and
[SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)

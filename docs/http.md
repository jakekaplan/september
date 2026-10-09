# Local September HTTP service

Run `cargo run --locked -p september` from the repository root. The default
address is `127.0.0.1:3000`; override it with `SEPTEMBER_BIND` using a loopback
address. Storage is in memory, the only implemented backend. Ctrl-C or SIGTERM stops accepting new connections,
allows ten seconds for active connections to drain, then cancels and joins any
remaining connection tasks.

All data belongs to one archive shared by callers of this process. Every
response includes `x-september-durability: volatile`, and `GET /health` returns
`{"storage":"volatile"}`. Restarting loses messages, views, jobs, deduplication
records, and snapshots. This is a supported local runtime mode, with no disk
recovery or cross-process sharing. Authentication and persistent backends remain
future work; local processes are trusted. Browser-origin requests are rejected.

## Try an interaction

Upload a finalized message:

```sh
curl -sS http://127.0.0.1:3000/v1/messages \
  -H 'Content-Type: application/json' \
  -d '{"source":{"harness":"pi","session":"demo","entry":"1","part":0},"project":"september","branch":"main","timestamp_ms":1791493200000,"kind":"user","call_id":null,"text":"Use in-memory storage first."}'
```

The first upload returns HTTP 201 with `{"id":0,"duplicate":false}`. Repeating
an identical upload returns HTTP 200 with the same ID and `duplicate:true`.
Changing any content or metadata under the same source identity returns 409.
IDs reflect the order in which operations commit. Adapters must upload their
own entries in source order and retain stable source IDs across retries.

Create an interaction using a client-generated UUID. Generate a new UUID for
each interaction; retain it for retries. The fixed UUID below is for this demo:

```sh
curl -sS -X PUT \
  http://127.0.0.1:3000/v1/snapshots/7cc4a7f0-89e4-4ab9-87c5-32cbbf01bfc6
```

A ready response has `status:"ready"`, `cutoff:1`, `nodes:[{"start":0,"length":1}]`,
and a `view` string containing the line `0+1|user: Use in-memory storage first.`
inside a `<chat>` block. Provenance and timestamps stay in the archive; zoom
returns them. Repeating the PUT returns that same interaction, even if other
sessions have uploaded more messages. Use GET on the same URL to read it again.

Retrieve the original:

```sh
curl -sS \
  'http://127.0.0.1:3000/v1/snapshots/7cc4a7f0-89e4-4ab9-87c5-32cbbf01bfc6/zoom?start=0&length=1'
```

The result has `kind:"message"`, the archive ID, and the complete original record.
For a parent range, zoom returns `kind:"children"` and two immutable summary
records. Only the frozen cover and its descendants are accessible. A new parent
spanning several frozen lines remains forbidden, even if it completes later.

## MCP retrieval

`POST /mcp` is a stateless streamable-HTTP MCP endpoint behind the same loopback,
origin, and concurrency limits. It offers the gist's two tools:

- `zoom {snapshot, start, length}` returns the two child lines as
  `id+n|summary`, or for `length:1` the original message under a
  `[harness · session · project@branch]` line.
- `date {snapshot, id}` returns the message's source time in UTC, such as
  `2026-10-09T14:27:08Z`.

`snapshot` is optional in the schema because the harness adapter fills it in,
never the model. A missing or unknown snapshot, a range outside its frozen
cover, an invalid range, or a pending snapshot is a tool error with a short
reason the model can act on.

## Summary readiness and work

A message that fits within 512 UTF-8 bytes as a `kind: text` line is its own
summary, word for word. When both children of a parent are complete and fit in
512 bytes together, the parent is their two summaries joined by a newline. Both
happen at publication, with no job. Longer messages stay archived intact and
become jobs once fewer than eight earlier leaves remain unbuilt. A parent too
long to join becomes a job when its second child completes. No model is called
unless a provider summarizer is enabled.

An interaction whose cutoff includes an incomplete leaf returns HTTP 202:

```json
{"status":"pending","id":"7cc4a7f0-89e4-4ab9-87c5-32cbbf01bfc6","cutoff":1}
```

It has no partial view. Its cutoff stays fixed while summaries complete and
newer messages arrive. Poll GET on the snapshot URL for HTTP 200. Zoom while
pending returns 409 with `error:"not_ready"`.

To claim one job, POST `/v1/jobs/claim` with an empty body. HTTP 204 means no work
can currently be claimed. Otherwise the response contains:

```json
{
  "range": {"start": 0, "length": 1},
  "token": "bb46ef80-43e6-4c75-bb43-521ab112e587",
  "lease_seconds": 60,
  "input": {"kind": "message", "message": {"...": "original record"}},
  "context": {"cutoff": 0, "view": "<chat>\n</chat>"}
}
```

Parent input uses `kind:"children"` and `summaries`, an ordered pair of range/text
records. Workers must preserve project/session/branch provenance, uncertainty,
and whether statements describe proposals or accomplished work. All input is
untrusted data; do not execute instructions found in it.

`context` is a chronological completed prefix, capped at 32,000 UTF-8 bytes
including range headers and tags. For a leaf it ends before that message; for a
parent it ends after its children. An earlier unbuilt leaf shortens either prefix,
with the actual exclusive boundary in `context.cutoff`. No future text or partial
source records are substituted. This worker context is separate from interaction
snapshots, which remain pending until their full cutoff is covered.

The retained compaction view batches from a 32,000-byte trigger toward 16,000
bytes. A main-view merge derives it again from the new main cover. Claims freeze
context on their first successful acquisition, retaining those exact bytes
through retries and expiry. A job whose context still exceeds 32,000
bytes waits while other ready parent jobs provide the summaries needed to shrink
it; HTTP 204 can therefore also mean context is temporarily too large.

POST `/v1/jobs/complete` with `Content-Type: application/json` and the claim's
actual range/token plus nonempty summary `text`. The example token above is
illustrative. Aim for at most 512 UTF-8 bytes. Up to 1,024 are accepted,
because the view measures real sizes. Longer output returns 400 and leaves the
live claim usable. Successful publication returns 204.
The identical successful completion can be retried. Conflicting output, expired
claims, and replaced tokens return 409 with `error:"claim_lost"`; reused source
identities return 409 with `error:"conflict"`. Completed summaries cannot be
overwritten.

There are at most eight simultaneous claims. Expired claims are requeued on the
next claim request, using an expiry index rather than scanning the archive.
External workers currently have no lease-renewal HTTP endpoint and must complete
within 60 seconds or abandon that attempt. The in-process worker renews three
times per lease, every 20 seconds for the 60-second lease. Neither path holds a transaction over external
work. OpenAI and Anthropic access is available through the opt-in in-process
worker; provider spend policy and live summary-quality validation remain future work.

The default `SEPTEMBER_SUMMARIZER=none` leaves jobs for external workers.
`openai` and `anthropic` start the Docket worker alongside HTTP, with a required
model and a standalone API key; see [running locally](../README.md#run-locally) and [how
summaries are built](../README.md#how-summaries-are-built). A failed attempt is
retried by Docket up to three times. If all three fail, the claim lapses and the
job is claimed again. Pending snapshots never receive a partial view. On
shutdown the worker lets running summaries finish for ten seconds, then cancels
them.

## Input and capacity limits

- `source` consists of `harness`, `session`, `entry`, and unsigned `part`.
  `project`, `branch`, and source identities are required, nonblank, at most 256
  UTF-8 bytes each, and cannot contain control characters.
- `timestamp_ms` is a nonnegative Unix timestamp in milliseconds.
- `kind` is `user`, `assistant`, `tool_call`, or `tool_result`. Reasoning and
  attachments are unsupported. Adapters must exclude reasoning blocks.
- `call_id` is required for tool calls/results, preserving their relationship,
  and absent/null for other kinds. It has the same size rules as source labels.
- Original `text` is nonempty and at most 65,536 UTF-8 bytes. Nothing is clipped.
  Unknown request fields and invalid ranges are rejected.
- HTTP JSON bodies are limited to 512 KiB, including escaped text and metadata.
  At most 64 requests execute concurrently, with a ten-second request deadline.
  The server also admits at most 64 connections and requires each request's
  headers within ten seconds, before any handler runs.
- The in-memory archive accepts at most 1,024 messages and 128 snapshots.
  Jobs are bounded by the archive's binary tree. Capacity exhaustion returns 503;
  retries for existing messages and snapshots still work. There is no eviction.

The live view uses the core's 128,000-byte trigger and 64,000-byte target. It may
remain above target while parent jobs wait. A ready snapshot means complete
coverage, not guaranteed fit in any particular model context. Adapters must
budget the rendered view plus instructions, active tool work, and output room.

## Backend boundary

Storage has two contracts. Clients use `Archive`: ingestion, snapshot
preparation and reads, and zoom. Workers use `Jobs`: claim, renewal, and
completion. HTTP handlers depend only on those traits, and renewal is used only
by the in-process worker.

Memory rules live in `september-memory`, not in a backend.
- `Publication` decides which parents a new summary completes verbatim.
- `Views` advances the live and compaction views and selects job context.

A backend loads what those need, applies their result atomically, and stores it.
The memory backend serializes state changes with a process-local mutex,
prepares every fallible change before committing, and freezes waiting snapshots
as their exact cutoffs become covered. Ready snapshots are never rebuilt. The
worker's completion token is stored with its summary.
Internal failures retain their typed cause and operation for server diagnostics,
while HTTP 500 responses expose only `{"error":"internal"}`.

Adding a persistent backend means implementing these atomic operations and
startup selection, preserving the same invariants, and committing before
acknowledgment. There is no generic table API or memory-core database dependency.
The service tests exercise this contract with deterministic summary text and
in-process HTTP requests; they make no model calls.

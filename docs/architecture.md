# September architecture

September is a hosted conversation-memory service. It owns one shared archive
for its initial user, while multiple sessions and harnesses contribute to that
archive. Memory is shared; active tool loops remain separate.

**The hosted system below is intended design.** The [README](../README.md#status)
is the single statement of what is implemented. In short:

- `september-memory` owns every rule that does not depend on storage. It holds
  only the summaries in the current views, never the full tree, and looks up
  built parents through a closure, so an unbuilt parent is simply absent.
  - `View` batch-merges, reports how many merges each update made, and keeps an
    unfinished batch's shrinking state.
  - `Views` keeps the compaction view following the live view and selects each
    job's context.
  - `Publication` joins short pairs verbatim and names the first parent that
    needs a model.
  - A `Snapshot` zooms only into its frozen cover lines and their descendants.
- `september` splits storage into `Archive` for clients and `Jobs` for workers.
  The in-memory backend only stores state and calls the core. It prepares a
  whole publication, then commits it: the summaries, both views, and any
  snapshot waiting on that cutoff. A Postgres backend would do the same inside a
  transaction, preloading the summaries the core's lookups need.
- Claims are fenced by token, renewable for 60 seconds at a time, and capped at
  eight. A job's compaction-view context freezes on its first claim and
  survives expiry. Jobs whose context exceeds 32,000 bytes wait while other
  parents shrink it. Ready jobs come from queues and an expiry index, never a
  tree scan.
- The Docket worker depends only on `Jobs` and receives its queue from the
  caller, so moving Docket to Redis is a setting. It dispatches claims as tasks,
  renews three times per lease the claim grants, and leaves retries and timeouts
  to Docket. On shutdown it lets running summaries finish for ten seconds, then
  cancels them; their claims lapse and are claimed again.

## Service boundary

```text
Harness adapters / CLI / MCP clients
                 |
                 v
       API server + authentication
                 |
          +------+-------+
          v              v
       Postgres     Attachment storage
          ^
          |
  Docket workers (Redis) ---> Model provider
```

The first hosted deployment should have an API process and Docket workers on
Redis, with Postgres holding all durable state. Postgres stays authoritative for
readiness, claims, and publication; Redis only carries Docket's work. Attachment
storage can be deferred for a text-only prototype, but image retrieval requires
it. Embeddings and a vector database are not part of this design.

### API process

The HTTP interface handles automatic harness integration: message ingestion,
snapshot preparation, view retrieval, zoom, timestamps, and readiness reporting.
An MCP endpoint exposes retrieval over the same service operations, not a second
memory engine. A future CLI uses HTTP when a harness cannot use MCP.

Adapters own capturing finalized messages, flushing pending uploads, identifying
interaction boundaries, constructing model context, and binding retrieval to a
snapshot. A skill or an MCP connection alone cannot guarantee automatic capture
or replace a harness's conversation history.

The service must distinguish a ready snapshot from one waiting on summaries.
Uploads are acknowledged after durable storage; summary completion is separate.
The initial text-only HTTP routes and schemas are defined in [http.md](http.md);
MCP and hosted authentication are not implemented yet.

### Postgres

The planned Postgres backend will own archived messages, stable source identities,
provenance, summary
nodes, persisted views, snapshots, durable summary jobs, and model usage.

Use transactions for ID assignment, deduplication, snapshot creation, and job
claims. Retried uploads refer to stable source identities and do not create new
messages. Reject a reused source identity with different content rather than
silently rewriting history. Preserve source ordering and tool-call/result
relationships even when different clients' uploads interleave.

The original archive is append-only and completed nodes are immutable. Views,
job claims, and snapshot readiness are mutable service state. A ready snapshot's
cutoff and node references must not change. No migrations or SQL are implemented
yet.

### Background worker

Docket runs the background work, independently of open client sessions. A
perpetual dispatch task claims ready jobs from Postgres and adds one summary
task per claim, keyed by its fencing token. A summary task renews its claim
while the model runs and publishes under that claim. A parent is published only
after both children are committed, and publication enforces node uniqueness and
fencing. Reading an already published parent must not require hydrating its
descendants. Ready jobs come from a durable queue; never scan the whole tree.

Up to eight summaries are built at once, enforced by storage across all workers.
Claims expire, so a lost worker or a lost Docket task is replaced without
accepting its stale completion. No transaction or database lock spans a model
call. Adding a claimed task to Docket is not atomic with the claim; a claim that
never reaches Docket lapses and is claimed again.

The intended starting model is GPT-6 Luna, selected explicitly with
`SEPTEMBER_SUMMARIZER=openai` and `SEPTEMBER_MODEL=gpt-6-luna`. Anthropic is also
supported with its own model and `ANTHROPIC_API_KEY`. A client's Codex or other
harness login is never used. Live model access and summary quality still need
validation.

Hosted model calls need timeouts, bounded retries, and recorded usage. The local
worker has timeouts and retries per claim, but no usage ledger or spend cap.
Automated tests use local provider fixtures and never make paid calls.
Summarization treats inputs as data and executes no tools.

### Attachments

Use S3-compatible object storage for images and large binary payloads, with
references in Postgres. Zooming to an original message can return its images.
Authorize access through the service; do not treat an arbitrary supplied URL or
filesystem path as a valid attachment location. Define retention and backup
behavior before accepting real attachments.

## Memory algorithm

The design comes from [UniiChat](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449),
which extends [OptMem](https://github.com/VictorTaelin/OptMem) from agent-selected
notes to automatic conversation capture.

1. Archive user messages, assistant replies, tool calls, and tool results
   supplied by adapters. Exclude reasoning blocks. Preserve source timestamps
   and harness, session, project, and branch provenance.
2. Compress each message into a leaf summary targeting at most 512 UTF-8 bytes.
   A message that fits as a `kind: text` line is its own summary, with no model
   call; two children that fit together are joined by a newline.
3. Merge adjacent aligned siblings into a parent targeting the same size. A
   level-L node covers 2^L messages, starting at a multiple of 2^L.
4. Maintain a chronological view covering the entire selected archive prefix.
   Append new leaf lines; do not continuously refit the view.
5. Once the rendered view exceeds 128,000 bytes, merge eligible sibling pairs
   in one batch toward 64,000 bytes. Only merge when the parent exists.
6. Rank merge candidates by age measured from the pair's last message,
   normalized by its child range size. Pick the oldest on ties.
7. Persist the live view. Restarting loads it rather than choosing a new cover.

The worker's compaction view follows its own 16,000–32,000-byte sawtooth and
respects each job's historical cutoff. Models cannot reliably count bytes, so
the worker measures output, shows the ruler and the cut, and tries at most five
times. As in the gist, it keeps the shortest draft even if it is slightly
oversized, so view accounting uses actual rendered byte lengths. Storage caps a
summary at 1,024 bytes, and the summarizer cuts a longer draft at that ceiling.
One node therefore never blocks every later snapshot.

The gist's harness-side rules belong to adapters, not this service: a fresh
model call per interaction, the view placed after fixed tools and instructions,
prompt-cache marks on whole blocks of four lines, and the `zoom` and `date`
tools.

Splitting long non-tool messages must preserve all retained content and source
identity across parts. The gist clips tool output to a 30,000-character head and
tail; any clipping must be explicit and must not be described as lossless
archival. Adapters also need to account for output already truncated by a
harness. Source retention and privacy policy remain separate decisions.

## Interaction snapshots

At the start of an interaction:

1. Flush that client's finalized prior messages and wait for their durable
   acknowledgment.
2. Freeze a global archive cutoff, independent of uploads arriving afterward.
3. Wait for the summaries needed to represent that cutoff, then freeze using the
   expected cutoff and durably save the immutable snapshot. A different cutoff
   is an explicit error, not a silently stale result.
4. Construct model context from harness instructions, the snapshot view, and the
   new message. Do not resend previous interactions verbatim.
5. Preserve the current interaction's messages and tool loop until it finishes.

All zoom and date requests are checked against the snapshot's archive prefix.
Zoom additionally requires a frozen cover node or its descendant, not an
arbitrary ancestor below the cutoff. The core selects child ranges or an original
message ID; the service retrieves their immutable contents, with pagination and
attachments as needed. Later merges and newly completed ancestors cannot expand
the frozen interaction's permitted navigation.

Other sessions keep uploading and summarizing throughout this interaction.
Their newer history becomes visible at the next interaction, not halfway
through the current task. Steering messages stay within the current interaction;
a queued follow-up acquires a new snapshot when it actually begins.

Global chronological order does not make every statement globally applicable.
Project provenance and progress states must survive summarization: proposals,
experiments, successes, and abandoned work are distinct. Shared memory provides
awareness, not filesystem locking or an inter-agent messaging bus.

## Rust ownership

```text
september service crate
  +-- HTTP / MCP boundary
  +-- service operations
  +-- Postgres persistence and durable jobs
  +-- model and attachment access
  +-- process configuration and lifecycle
  |
  +--> september-memory crate
         +-- aligned ranges and tree rules
         +-- view merge selection and byte accounting
         +-- snapshot coverage and permitted zoom ranges
```

Pure rules belong in `crates/memory`; effects and orchestration belong in
`crates/service`. These are responsibilities, not pre-created modules or traits.
Add modules alongside implementation and extract another crate only when a
concrete dependency or ownership boundary warrants it. The local server
entrypoint and model worker share service storage operations. In-memory
deployments must share one backend in one process; a
separate worker process requires the HTTP interface or persistent shared storage.
No client SDK or harness adapter belongs in the memory engine.

## Reliability and security

Use bounded uploads and work queues, cancellation, and explicit failures. Record
queue age, summary failures, model usage, and request latency. Keep metric labels
bounded; archive IDs and session IDs must not become high-cardinality metric
labels. Structured diagnostics should not include conversation bodies or secrets
by default. See `../SECURITY.md` for trust boundaries and operational requirements.

Transactions and idempotency provide durable ingestion, not an end-to-end
exactly-once delivery claim. Clients need durable outbound queues for failures
and reconnects. A cached view is stale fallback memory, not a fresh global
snapshot; offline behavior must be explicit rather than silently equivalent.

A byte budget preserves the gist's view behavior but does not guarantee that a
model request fits. Adapters must budget instructions, the active tool loop,
retrieved content, and output allowance against the chosen model's token and
request limits. Long-interaction overflow handling needs an explicit policy.

Preserving an append-only archive does not prevent summary omissions or guarantee
that a model will find the right evidence. Test retrieval quality separately
from tree math. Prompt-cache benefits also depend on provider, model, tool set,
and harness instructions; do not present simulated Anthropic hit rates as measured
Codex or cross-harness savings.

## Decisions before implementation

- Validate live GPT-6 Luna access and summary quality using the standalone API key.
- Specify normalized message/source identities, branch lifecycle handling, and
  versioned HTTP/MCP contracts.
- Select retention, export, deletion, and attachment policies before storing
  real conversations. The initial rollout does not bulk-import old sessions.
- Define offline behavior, blocked-snapshot failures, long-interaction overflow,
  and spending limits.
- Select a project license and private security-reporting channel before release.

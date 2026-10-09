# September architecture

September is a hosted conversation-memory service. It owns one shared archive
for its initial user, while multiple sessions and harnesses contribute to that
archive. Memory is shared; active tool loops remain separate.

**The hosted system below remains intended design.** The deterministic memory
core and a local HTTP service with in-memory storage are implemented. Persistent
storage, authentication, MCP, and adapters remain future work. A continuous
worker supports opt-in OpenAI, Anthropic, and fake summarizers.

## Implemented local service

`september` exposes a versioned HTTP interface over the `Storage` contract.
`InMemory` implements atomic ingestion/deduplication, immutable summary
publication, retained live views, fixed-cutoff snapshots, and original retrieval.
It maintains a ready queue with at most eight recoverable claims, a 60-second
renewable lease and token fencing. Parent jobs are enqueued only when both children exist.
The queue and all other state are volatile, bounded, and shared in one process.

Short source records, including provenance, become verbatim leaves when their
serialized text fits within 512 UTF-8 bytes. Other leaves and all parents require
explicit summary completion by default. An opt-in worker completes them using
a model provider or a visibly fake callback for synthetic data. Pending snapshots
hold their original cutoffs; the live view advances through completed leaves and
freezes waiting snapshots at those exact boundaries, without rebuilding a cover.

The server binds only to loopback, rejects browser-origin/nonlocal-host requests,
and reports volatile storage on every response. See [HTTP contracts and a runnable
walkthrough](http.md). Future SQLite/Postgres implementations belong behind the
same atomic operations. Backend selection is a startup choice; currently only
`memory` is available. In-memory mode is an explicit exception to hosted durability:
acknowledgment confirms an atomic process-local commit, not survival of a restart.

The continuous worker owns a private Docket `memory://` queue, polls ready work
every 250 ms, and keeps at most eight claims in flight. Task keys are claim
tokens. September renews claims every 20 seconds across execution and retries,
independently of Docket's delivery lease. Each attempt times out after two minutes;
three failures release the claim with a 30-second delay. A delayed-ready index
provides backoff without scanning archive nodes. Infrastructure failures stop
the worker and server after cleanup; failed summaries remain visibly pending.

Shutdown stops dispatch, drains for ten seconds, then cancels remaining work
and releases its claims. Release and queue cancellation have five-second limits
each and run concurrently across the bounded set of claims. Abrupt worker loss
recovers through claim expiry; whole-process loss still erases all state. The
fake callback marks its output explicitly and does not preserve conversation
meaning. Real inference uses `genai` with an explicit OpenAI Responses or Anthropic
Messages target and standalone API key. Typed startup settings use `config` and
Serde, with optional TOML followed by environment overrides. See the
[setup instructions](../README.md#continuous-worker).

The model callback joins short parent children verbatim when their text plus a
newline fits within 512 UTF-8 bytes. Otherwise it sends immutable input and frozen
context as untrusted data, preserving attribution, provenance, and progress states
in its instructions. It accepts only complete plain-text output, excludes reasoning,
and measures UTF-8 bytes. An oversized draft gets up to five corrections; failure
leaves the job pending through the existing retry path. There is no truncation or
oversized fallback. Requests have a 90-second timeout and 2,048-output-token cap;
the worker's two-minute attempt deadline also covers all corrections. OpenAI
response storage is disabled. Raw provider errors are discarded because they can
contain conversation bodies or credentials. Tests use local provider fixtures;
live summary quality and provider spend policy remain unvalidated/unimplemented.

Publication advances both the retained main view and a smaller compaction view.
The smaller view batches at 32,000 bytes toward 16,000 bytes; main-view merges
rederive it from the new main cover and immediately batch toward its target.
A claim takes only whole cover lines before its leaf, or through its parent’s
children, stopping at the first unbuilt leaf. An unbuilt job cannot have a
completed ancestor, so its historical boundary never splits a retained node.
The actual cutoff accompanies the rendered context. This prefix is auxiliary
worker context; it does not make an unready interaction snapshot ready.

The first successful claim saves that job’s context across retries, release, and
expiry. If ready summaries cannot bring context within 32,000 UTF-8 bytes, claim
selection skips that job and visits other entries in the ready queue. It never
scans the archive tree. Leaves enter that queue only when fewer than eight earlier
leaves remain unbuilt, including those running or backing off. Tests exercise
out-of-order publication, retry stability, UTF-8 limits, and parent progress
when the smaller view is initially oversized.

## Implemented memory core

`september-memory` provides checked `Node` ranges, immutable completed `Summary`
values, byte `Budget`s, batched `View`s, and frozen `Snapshot`s. It has no full-
history tree collection, external dependencies, I/O, model calls, original-message
storage, or persistence format. The service must verify immutable publication,
unique node identity, and child readiness before supplying completed records;
the memory core does not implement those archive guarantees.

A view owns only its current summary cover. Compaction takes a read-only
`BTreeMap<Node, String>` of ready-parent text, not a backend interface. Supply all
completed parents eligible in the operation, including those enabled by earlier
merges in the batch. An absent entry means not ready, never an unchecked cache
miss. Only the selected parent is copied into an immutable summary value. View
updates prepare one owned candidate and commit once after successful compaction.

Views render `id+n|text` lines inside `<chat>` tags, flattening CR/LF to spaces
and counting every actual UTF-8 byte, including headers, separators, and tags.
Merge ranking uses inclusive last-message IDs and exact integer comparisons,
with oldest-first ties.

A triggered batch retains its shrinking state until it reaches the target, even
if available merges bring it below the trigger first. Save its node identities
and this state together, then load only the referenced completed summaries.
`View::prefix` selects only complete cover lines at an exact historical boundary.
`View::resize` derives a new budgeted cover and retains unfinished batch state.
`View::restore` validates coverage and state against the same budget without
refitting or merging. Loading a single `0+1024` summary does not require its
2,046 descendants. Persistence remains the service's responsibility.

Freezing requires an explicit expected exclusive cutoff and complete summary
coverage through it. No missing leaf is replaced by a placeholder or stale view.
A complete view may still have a blocked shrinking batch; callers must separately
enforce their model's context limit. Snapshots own rendered text and a node cover,
with the cutoff derived from that cover. Zoom selects only a frozen cover node
or its descendants, returning two child ranges or an original-message ID. A
later-completed ancestor spanning multiple cover lines remains ineligible, even
below the cutoff. Content lookup, archive identity, and authorization belong
to the service; there is no live-tree dependency in frozen navigation.

Tests cover merge ranking, hysteresis, unfinished-batch recovery, unchanged
restoration, invalid inputs, UTF-8 accounting, and snapshot isolation across new
messages and live merges. They use supplied summaries, not real model output,
and do not establish summarization or retrieval quality.

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
    Summary worker ---> Model provider
```

The first hosted deployment should have an API process and a background worker,
with Postgres coordinating durable state. Attachment storage can be deferred
for a text-only prototype, but image retrieval requires it. A separate queue
service, Redis, embeddings, and a vector database are not part of this design.

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

The worker runs independently of open client sessions. It claims ready jobs,
builds summaries, saves immutable nodes, and schedules parents when dependencies
become available. Publish a parent only after both immutable child records are
committed, and enforce node uniqueness and lease fencing at publication. Reading
an already published parent must not require hydrating its descendants. Use a
durable ready queue; never scan the whole tree for work.

Bound concurrent model calls to eight initially. Claims need expiry and fencing
so a stopped worker can be replaced without accepting a stale completion. No
transaction or database lock spans a model call. Graceful shutdown stops new
claims and drains or releases in-flight work without acknowledging lost data.

The intended starting model is GPT-6 Luna, selected explicitly with
`SEPTEMBER_SUMMARIZER=openai` and `SEPTEMBER_MODEL=gpt-6-luna`. The local worker
uses the OpenAI Responses endpoint and `OPENAI_API_KEY`; Anthropic is also
supported with its own model setting and `ANTHROPIC_API_KEY`. Live model access
and summary quality still need validation. A client's Codex login is never used.

Hosted model calls need timeouts, bounded retries, and recorded usage. The local
worker implements timeouts and retry limits per claim, but has no usage ledger
or aggregate spend cap. Automated tests
use fake model responses and never make paid calls. A separate small, persisted
compaction view provides historical context without revealing future messages.
Summarization interprets inputs as data and executes no tools.

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
   Short sources can be retained verbatim without a model call.
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
respects each job's historical cutoff. Models cannot reliably count bytes:
measure output, show the ruler and cut, and retry oversized results up to five
times. The gist keeps the shortest result even if slightly oversized; view
accounting must therefore use actual rendered byte lengths, not 512 times the
number of lines. Final failure policy needs to preserve readiness truthfully.

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

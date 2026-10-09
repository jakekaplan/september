# Security policy

September currently runs a local, unauthenticated HTTP service with volatile
in-memory storage. The executable requires a loopback bind. HTTP requests have
body, request/connection concurrency, header-read and handler time limits;
shutdown has a bounded drain period. Browser Origin headers and nonlocal Host
headers are rejected. Local processes are trusted, including callers of summary
claim/completion endpoints. Do not expose this version through a public proxy.
There is no persistent database or hosted deployment. The remaining rules below
are requirements for hosted operation, not claims of implemented protection.

## Trust boundaries

- Clients and archived conversation contents are untrusted. Validate upload
  sizes, identities, formats, ranges, and snapshot access at service boundaries.
- Summaries are navigation aids, not authoritative instructions. Workers must
  treat inputs as data, never execute tools from archived instructions, and must
  not infer that an attempted action succeeded.
- Every API, MCP, and attachment request needs authorization for the relevant
  archive. A snapshot identifier or source identifier is not a credential.
- Separate retrieval access from ingestion and administrative permissions. MCP
  retrieval tools must not become a route to arbitrary database or filesystem
  access.
- Attachment references must not enable arbitrary URL fetching, path traversal,
  or access to another archive's objects.

## Sensitive information

Conversation messages and tool output may include private code, personal data,
and credentials. Do not log bodies, authorization headers, model credentials,
or signed attachment URLs by default. Prefer bounded metadata such as request
IDs, byte counts, operation names, and error categories.
Internal service failures retain their original typed cause and operation for
server diagnostics. Those causes must exclude conversation bodies and credentials;
the HTTP error response remains sanitized.

Use synthetic data in tests and examples. Never commit real conversations or
credentials. Local `.env` files and service data are ignored; review diffs and
exports anyway. Ignore rules are not a security boundary.

The hosted worker needs its own model authentication. Do not forward a client's
harness credentials automatically or assume a local subscription login works
as a service credential. Model providers receiving archived content are part
of the data-handling trust boundary.

## Operational requirements

Use HTTPS, bounded uploads and work queues, model timeouts, and spend limits.
Persistent backends acknowledge uploads only after durable storage. The local
in-memory backend explicitly returns volatile acknowledgments. Back up the database and
attachments, restrict backup access, and test restoration. Define retention,
export, and deletion behavior before storing real conversations in production;
append-only history alone is not a complete privacy policy.

CI must not run untrusted pull-request code with service credentials. Dependency
policy changes and exceptions need specific reasons. Keep actions pinned to
verified release commits and the dependency lockfile under review.

## Reporting

Do not post sensitive vulnerabilities, credentials, or conversation contents in
public issues. Contact the repository owner privately. A public reporting
channel and response policy must be selected before a public service launch.

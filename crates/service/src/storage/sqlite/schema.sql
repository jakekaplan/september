-- September's SQLite archive, schema version 1.

-- The append-only archive, in ID order. `body` is the whole message as JSON.
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    harness TEXT NOT NULL,
    session TEXT NOT NULL,
    entry TEXT NOT NULL,
    part INTEGER NOT NULL,
    body TEXT NOT NULL,
    UNIQUE (harness, session, entry, part)
) STRICT;

-- Immutable completed summaries. `token` is the claim that published a model
-- summary; verbatim summaries have none.
CREATE TABLE summaries (
    start INTEGER NOT NULL,
    length INTEGER NOT NULL,
    text TEXT NOT NULL,
    token TEXT,
    PRIMARY KEY (start, length)
) STRICT, WITHOUT ROWID;

-- Summary work from ready until published, claimed in `seq` order. A job's
-- context is frozen on its first claim and kept across expiry and restarts.
CREATE TABLE jobs (
    seq INTEGER PRIMARY KEY,
    start INTEGER NOT NULL,
    length INTEGER NOT NULL,
    context_cutoff INTEGER,
    context_view TEXT,
    UNIQUE (start, length)
) STRICT;

-- Leaves without a summary yet; only the first eight are jobs.
CREATE TABLE unbuilt (
    start INTEGER PRIMARY KEY
) STRICT;

-- The live and compaction views, saved with their batch state. `nodes` is the
-- cover as JSON `[[start, length], ...]`; its text is in `summaries`.
CREATE TABLE views (
    name TEXT PRIMARY KEY CHECK (name IN ('live', 'compaction')),
    nodes TEXT NOT NULL,
    shrinking INTEGER NOT NULL
) STRICT;

-- Interaction snapshots. `nodes` is the frozen cover, or NULL while waiting
-- for the live view to reach `cutoff`. `within` is the requested byte size.
CREATE TABLE snapshots (
    id TEXT PRIMARY KEY,
    cutoff INTEGER NOT NULL,
    within INTEGER,
    nodes TEXT
) STRICT;

CREATE INDEX waiting ON snapshots (cutoff) WHERE nodes IS NULL;

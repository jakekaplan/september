//! Single-process storage in SQLite: durable in a file, or volatile in memory.
//!
//! Each change is one immediate transaction: read the saved state it needs, let
//! the memory core decide, write the result, and commit before replying. Claims
//! are leases held by running workers, so they live in memory; after a restart
//! every unpublished job is claimable again, with its frozen context.

use std::{cell::Cell, fs, io, path::Path, sync::Arc, time::Duration};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use september_memory::{Budget, Node, Publication, View, Views, Zoom};
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    Error,
    archive::{Message, Receipt},
    error::Invariant,
    jobs::{Claims, MAX_ELIGIBLE_LEAVES},
    snapshots::{Detail, Snapshot, Summary},
    storage::Archive,
};

mod jobs;
mod snapshots;

const SCHEMA: &str = include_str!("sqlite/schema.sql");
const SCHEMA_VERSION: i64 = 1;

/// Storage in one SQLite database, held exclusively by this process until
/// dropped. Cloning an `Arc<Sqlite>` shares the archive.
pub struct Sqlite {
    state: Arc<Mutex<State>>,
    durable: bool,
}

struct State {
    db: Connection,
    /// The live view's budget; the compaction view always uses [`Budget::COMPACTION`].
    budget: Budget,
    claims: Claims,
}

/// Why the archive could not be opened at startup.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Its folder could not be created.
    #[error("could not create its folder: {0}")]
    Folder(#[source] io::Error),
    /// SQLite refused it, including when another server holds it.
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    /// Its schema version is not one this server knows.
    #[error("unknown schema version {0}")]
    UnknownSchema(i64),
}

impl Sqlite {
    /// Open or create a durable archive at `path`, whose live view batches
    /// within `budget`.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError`] if the file cannot be opened, another connection
    /// holds it, or it has an unknown schema version.
    pub fn open(path: &Path, budget: Budget) -> Result<Self, OpenError> {
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder).map_err(OpenError::Folder)?;
        }
        let db = Connection::open(path)?;
        // Fail at once if another server holds the file instead of waiting for it.
        db.busy_timeout(Duration::ZERO)?;
        // Claims are fenced in this process, so it must be the file's only user.
        db.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        db.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        Self::start(db, budget, true)
    }

    /// Start an empty volatile archive, lost when dropped, whose live view
    /// batches within `budget`.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError`] if SQLite cannot create the database.
    pub fn in_memory(budget: Budget) -> Result<Self, OpenError> {
        Self::start(Connection::open_in_memory()?, budget, false)
    }

    fn start(mut db: Connection, budget: Budget, durable: bool) -> Result<Self, OpenError> {
        // A file's first write takes the exclusive lock, held from here on.
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            unknown => return Err(OpenError::UnknownSchema(unknown)),
        }
        tx.commit()?;
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                db,
                budget,
                claims: Claims::default(),
            })),
            durable,
        })
    }

    /// Run one operation on a blocking thread, holding the connection.
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut State) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || operation(&mut state.blocking_lock()))
            .await
            .map_err(|error| Error::internal("run database operation", error))?
    }
}

impl Archive for Sqlite {
    fn is_durable(&self) -> bool {
        self.durable
    }

    async fn ingest(&self, message: Message) -> Result<Receipt, Error> {
        message.validate()?;
        self.run(move |state| state.ingest(&message)).await
    }

    async fn prepare(&self, id: Uuid, within: Option<Budget>) -> Result<Snapshot, Error> {
        self.run(move |state| state.prepare(id, within)).await
    }

    async fn snapshot(&self, id: Uuid) -> Result<Snapshot, Error> {
        self.run(move |state| {
            let (cutoff, frozen) = snapshots::saved(&state.db, id)?;
            Ok(Snapshot::new(id, cutoff, frozen.as_ref()))
        })
        .await
    }

    async fn zoom(&self, id: Uuid, node: Node) -> Result<Detail, Error> {
        self.run(move |state| {
            let (_, frozen) = snapshots::saved(&state.db, id)?;
            match frozen.ok_or(Error::NotReady)?.zoom(node)? {
                Zoom::Message(id) => Ok(Detail::Message {
                    id,
                    message: message(&state.db, id)?,
                }),
                Zoom::Children([left, right]) => Ok(Detail::Children {
                    summaries: [summary(&state.db, left)?, summary(&state.db, right)?],
                }),
            }
        })
        .await
    }
}

impl State {
    fn ingest(&mut self, message: &Message) -> Result<Receipt, Error> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source = &message.source;
        let prior = tx
            .prepare_cached(
                "SELECT id, body FROM messages
                 WHERE harness = ?1 AND session = ?2 AND entry = ?3 AND part = ?4",
            )?
            .query_row(
                params![source.harness, source.session, source.entry, source.part],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((id, body)) = prior {
            if decode::<Message>(&body)? != *message {
                return Err(Error::Conflict);
            }
            return Ok(Receipt {
                id,
                duplicate: true,
            });
        }
        let id = next_id(&tx)?;
        tx.prepare_cached(
            "INSERT INTO messages (id, harness, session, entry, part, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(params![
            id,
            source.harness,
            source.session,
            source.entry,
            source.part,
            encode(message)?
        ])?;
        let node = Node::new(id, 1)?;
        let published = if let Some(text) = message.verbatim_summary() {
            let summary = september_memory::Summary::new(node, text);
            publish(&tx, self.budget, summary, None)?
        } else {
            tx.prepare_cached("INSERT INTO unbuilt (start) VALUES (?1)")?
                .execute([id])?;
            // Only the first eight unbuilt leaves are jobs.
            if nth_unbuilt(&tx, MAX_ELIGIBLE_LEAVES)?.is_none() {
                enqueue(&tx, node)?;
            }
            Vec::new()
        };
        tx.commit()?;
        self.release(&published);
        Ok(Receipt {
            id,
            duplicate: false,
        })
    }

    /// Drop claims on published jobs, once the publication has committed.
    fn release(&mut self, published: &[Node]) {
        for &node in published {
            self.claims.release(node);
        }
    }
}

/// Publish `summary` with every verbatim parent it completes: advance both
/// views, freeze the snapshots waiting at the cutoffs the live view reaches, and
/// enqueue the newly ready work. Returns the published nodes.
fn publish(
    tx: &Transaction<'_>,
    budget: Budget,
    summary: september_memory::Summary,
    token: Option<Uuid>,
) -> Result<Vec<Node>, Error> {
    let lookup = Lookup::new(tx);
    let publication = Publication::new(summary, |node| lookup.text(node));
    let built = |node| publication.text(node).or_else(|| lookup.text(node));
    let mut views = load_views(tx, budget)?;
    let waiting = snapshots::waiting(tx)?;
    let reached = views
        .advance(built, |cutoff| waiting.contains(&cutoff))
        .map_err(|error| Error::internal("advance views", error))?;
    let frozen = snapshots::freeze_waiting(tx, &reached, built)?;
    lookup.check()?;
    // Only the supplied summary has a claim; joined parents are verbatim.
    let tokens = std::iter::once(token).chain(std::iter::repeat(None));
    let mut published = Vec::new();
    for (summary, token) in publication.summaries().iter().zip(tokens) {
        let node = summary.node();
        tx.prepare_cached(
            "INSERT INTO summaries (start, length, text, token) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            node.start(),
            node.length(),
            summary.text(),
            token.map(|token| token.to_string())
        ])?;
        tx.prepare_cached("DELETE FROM jobs WHERE start = ?1 AND length = ?2")?
            .execute([node.start(), node.length()])?;
        let was_unbuilt = node.length() == 1
            && tx
                .prepare_cached("DELETE FROM unbuilt WHERE start = ?1")?
                .execute([node.start()])?
                > 0;
        // Building a leaf lets the next unbuilt one become a job.
        if was_unbuilt && let Some(start) = nth_unbuilt(tx, MAX_ELIGIBLE_LEAVES - 1)? {
            enqueue(tx, Node::new(start, 1)?)?;
        }
        published.push(node);
    }
    save_views(tx, &views)?;
    snapshots::make_ready(tx, &frozen)?;
    if let Some(job) = publication.job() {
        enqueue(tx, job)?;
    }
    Ok(published)
}

/// Completed text for the core's `built` lookups. The core reads `None` as not
/// built, so a failed query is kept and reported by [`Self::check`] before
/// anything decided from it is written.
struct Lookup<'a> {
    db: &'a Connection,
    failure: Cell<Option<rusqlite::Error>>,
}

impl<'a> Lookup<'a> {
    fn new(db: &'a Connection) -> Self {
        Self {
            db,
            failure: Cell::new(None),
        }
    }

    fn text(&self, node: Node) -> Option<Arc<str>> {
        summary_text(self.db, node).unwrap_or_else(|error| {
            self.failure.set(Some(error));
            None
        })
    }

    fn check(self) -> Result<(), Error> {
        self.failure
            .into_inner()
            .map_or(Ok(()), |error| Err(error.into()))
    }
}

fn summary_text(db: &Connection, node: Node) -> rusqlite::Result<Option<Arc<str>>> {
    db.prepare_cached("SELECT text FROM summaries WHERE start = ?1 AND length = ?2")?
        .query_row([node.start(), node.length()], |row| {
            Ok(Arc::from(row.get::<_, String>(0)?))
        })
        .optional()
}

fn summary(db: &Connection, node: Node) -> Result<Summary, Error> {
    let text = summary_text(db, node)?
        .ok_or_else(|| Error::internal("retrieve summary", Invariant::MissingSummary(node)))?;
    Ok(Summary {
        range: node.into(),
        text: text.to_string(),
    })
}

fn message(db: &Connection, id: u64) -> Result<Message, Error> {
    let body: String = db
        .prepare_cached("SELECT body FROM messages WHERE id = ?1")?
        .query_row([id], |row| row.get(0))
        .optional()?
        .ok_or_else(|| Error::internal("retrieve original", Invariant::MissingMessage(id)))?;
    decode(&body)
}

/// The completed summaries of a saved cover, in order.
fn cover(db: &Connection, nodes: &[Node]) -> Result<Vec<september_memory::Summary>, Error> {
    nodes
        .iter()
        .map(|&node| {
            let text = summary(db, node)?.text;
            Ok(september_memory::Summary::new(node, text))
        })
        .collect()
}

fn load_views(db: &Connection, budget: Budget) -> Result<Views, Error> {
    let live = load_view(db, "live", budget)?;
    let compaction = load_view(db, "compaction", Budget::COMPACTION)?;
    Views::restore(live, compaction).map_err(|error| Error::internal("restore views", error))
}

fn load_view(db: &Connection, name: &str, budget: Budget) -> Result<View, Error> {
    let saved = db
        .prepare_cached("SELECT nodes, shrinking FROM views WHERE name = ?1")?
        .query_row([name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .optional()?;
    let Some((nodes, shrinking)) = saved else {
        return Ok(View::new(budget));
    };
    let summaries = cover(db, &decode_nodes(&nodes)?)?;
    View::restore(summaries, shrinking, budget)
        .map_err(|error| Error::internal("restore view", error))
}

fn save_views(db: &Connection, views: &Views) -> Result<(), Error> {
    save_view(db, "live", views.live())?;
    save_view(db, "compaction", views.compaction())
}

fn save_view(db: &Connection, name: &str, view: &View) -> Result<(), Error> {
    let nodes: Vec<Node> = view
        .summaries()
        .iter()
        .map(september_memory::Summary::node)
        .collect();
    db.prepare_cached(
        "INSERT INTO views (name, nodes, shrinking) VALUES (?1, ?2, ?3)
         ON CONFLICT (name) DO UPDATE SET nodes = excluded.nodes, shrinking = excluded.shrinking",
    )?
    .execute(params![name, encode_nodes(&nodes)?, view.is_shrinking()])?;
    Ok(())
}

/// The ID the next message gets, which is also the archive's cutoff.
fn next_id(db: &Connection) -> Result<u64, Error> {
    Ok(db
        .prepare_cached("SELECT COALESCE(MAX(id) + 1, 0) FROM messages")?
        .query_row([], |row| row.get(0))?)
}

/// The unbuilt leaf with `index` earlier unbuilt leaves, if there is one.
fn nth_unbuilt(db: &Connection, index: usize) -> Result<Option<u64>, Error> {
    Ok(db
        .prepare_cached("SELECT start FROM unbuilt ORDER BY start LIMIT 1 OFFSET ?1")?
        .query_row([index], |row| row.get(0))
        .optional()?)
}

fn enqueue(db: &Connection, node: Node) -> Result<(), Error> {
    db.prepare_cached("INSERT OR IGNORE INTO jobs (start, length) VALUES (?1, ?2)")?
        .execute([node.start(), node.length()])?;
    Ok(())
}

fn encode(value: &impl Serialize) -> Result<String, Error> {
    serde_json::to_string(value).map_err(|error| Error::internal("encode saved state", error))
}

fn decode<T: DeserializeOwned>(text: &str) -> Result<T, Error> {
    serde_json::from_str(text).map_err(|error| Error::internal("decode saved state", error))
}

fn encode_nodes(nodes: &[Node]) -> Result<String, Error> {
    let pairs: Vec<_> = nodes
        .iter()
        .map(|node| (node.start(), node.length()))
        .collect();
    encode(&pairs)
}

fn decode_nodes(text: &str) -> Result<Vec<Node>, Error> {
    decode::<Vec<(u64, u64)>>(text)?
        .into_iter()
        .map(|(start, length)| {
            Node::new(start, length).map_err(|error| Error::internal("decode saved state", error))
        })
        .collect()
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal("query database", error)
    }
}

#[cfg(test)]
mod tests;

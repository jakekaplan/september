use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use september_memory::{Budget, Node, Publication, Snapshot as Frozen, Views, Zoom};
use tokio::{sync::Mutex, time::Instant};
use uuid::Uuid;

use crate::{
    Error,
    archive::{Message, Receipt, Source},
    error::Invariant,
    jobs::{
        Claim, Completion, Context, Input, Job, LEASE, MAX_CLAIMS, MAX_ELIGIBLE_LEAVES,
        MAX_SUMMARY_BYTES,
    },
    snapshots::{Detail, Snapshot, Summary},
    storage::{Archive, Jobs},
};

mod snapshots;
use snapshots::Snapshots;

const MAX_MESSAGES: usize = 1024;

/// Volatile single-process storage, bounded to 1,024 messages and 128 snapshots.
/// Cloning an `Arc<InMemory>` shares the archive; constructing another starts empty.
pub struct InMemory {
    state: Mutex<State>,
}

struct State {
    messages: Vec<Message>,
    sources: BTreeMap<Source, u64>,
    summaries: BTreeMap<Node, Published>,
    ready: VecDeque<Node>,
    unbuilt: BTreeSet<Node>,
    contexts: BTreeMap<Node, Context>,
    claims: BTreeMap<Node, (Uuid, Instant)>,
    expirations: BTreeSet<(Instant, Node)>,
    views: Views,
    snapshots: Snapshots,
}

struct Published {
    text: Arc<str>,
    /// The claim that published a model summary; verbatim summaries have none.
    token: Option<Uuid>,
}

/// Every change one publication makes, prepared before any of them is applied.
struct Prepared {
    publication: Publication,
    token: Option<Uuid>,
    views: Views,
    frozen: Vec<(Uuid, Frozen)>,
}

impl InMemory {
    /// Start an empty archive whose live view batches within `budget`.
    #[must_use]
    pub fn new(budget: Budget) -> Self {
        Self {
            state: Mutex::new(State {
                messages: Vec::new(),
                sources: BTreeMap::new(),
                summaries: BTreeMap::new(),
                ready: VecDeque::new(),
                unbuilt: BTreeSet::new(),
                contexts: BTreeMap::new(),
                claims: BTreeMap::new(),
                expirations: BTreeSet::new(),
                views: Views::new(budget, Budget::COMPACTION),
                snapshots: Snapshots::default(),
            }),
        }
    }
}

impl Default for InMemory {
    fn default() -> Self {
        Self::new(Budget::CHAT)
    }
}

impl Archive for InMemory {
    fn is_durable(&self) -> bool {
        false
    }

    async fn ingest(&self, message: Message) -> Result<Receipt, Error> {
        message.validate()?;
        let verbatim = message.verbatim_summary();
        let mut state = self.state.lock().await;
        if let Some(&id) = state.sources.get(&message.source) {
            if state.message(id)? != &message {
                return Err(Error::Conflict);
            }
            return Ok(Receipt {
                id,
                duplicate: true,
            });
        }
        if state.messages.len() >= MAX_MESSAGES {
            return Err(Error::Capacity);
        }
        let id = u64::try_from(state.messages.len())
            .map_err(|error| Error::internal("assign archive cutoff", error))?;
        let node = Node::new(id, 1)?;
        let prepared = verbatim
            .map(|text| {
                let summary = september_memory::Summary::new(node, text);
                state.prepare_publication(summary, None)
            })
            .transpose()?;
        // Everything that can fail has been prepared before this commit.
        state.sources.insert(message.source.clone(), id);
        state.messages.push(message);
        if let Some(prepared) = prepared {
            state.publish(prepared);
        } else {
            state.unbuilt.insert(node);
            if state.unbuilt.len() <= MAX_ELIGIBLE_LEAVES {
                state.ready.push_back(node);
            }
        }
        Ok(Receipt {
            id,
            duplicate: false,
        })
    }

    async fn prepare(&self, id: Uuid, within: Option<usize>) -> Result<Snapshot, Error> {
        let within = within
            .map(Budget::at_most)
            .transpose()
            .map_err(|_| Error::Invalid)?;
        let mut state = self.state.lock().await;
        let cutoff = u64::try_from(state.messages.len())
            .map_err(|error| Error::internal("assign archive cutoff", error))?;
        let State {
            views,
            snapshots,
            summaries,
            ..
        } = &mut *state;
        let built = |node| built(summaries, node);
        snapshots.prepare(id, cutoff, within, views.live(), built)
    }

    async fn snapshot(&self, id: Uuid) -> Result<Snapshot, Error> {
        self.state.lock().await.snapshots.get(id)
    }

    async fn zoom(&self, id: Uuid, node: Node) -> Result<Detail, Error> {
        let state = self.state.lock().await;
        match state.snapshots.frozen(id)?.zoom(node)? {
            Zoom::Message(id) => Ok(Detail::Message {
                id,
                message: state.message(id)?.clone(),
            }),
            Zoom::Children([left, right]) => Ok(Detail::Children {
                summaries: [state.summary(left)?, state.summary(right)?],
            }),
        }
    }
}

impl Jobs for InMemory {
    async fn claim(&self) -> Result<Option<Claim>, Error> {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        // Only the expiry index is visited; never scan the archive for work.
        while let Some(&(deadline, node)) = state.expirations.first() {
            if deadline > now {
                break;
            }
            state.expirations.pop_first();
            state.claims.remove(&node);
            state.ready.push_back(node);
        }
        if state.claims.len() >= MAX_CLAIMS {
            return Ok(None);
        }
        let mut selected = None;
        for (index, &node) in state.ready.iter().enumerate() {
            // Let other ready parents shrink the context before admitting this job.
            if let Some(context) = state.context(node)? {
                selected = Some((index, node, context));
                break;
            }
        }
        let Some((index, node, context)) = selected else {
            return Ok(None);
        };
        let input = if let Some([left, right]) = node.children() {
            Input::Children {
                summaries: [state.summary(left)?, state.summary(right)?],
            }
        } else {
            Input::Message {
                message: state.message(node.start())?.clone(),
            }
        };
        let token = Uuid::new_v4();
        let deadline = now + LEASE;
        state.ready.remove(index);
        state.contexts.insert(node, context.clone());
        state.claims.insert(node, (token, deadline));
        state.expirations.insert((deadline, node));
        Ok(Some(Claim {
            job: Job {
                range: node.into(),
                input,
                context,
            },
            token,
            lease_seconds: LEASE.as_secs(),
        }))
    }

    async fn renew(&self, node: Node, token: Uuid) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let deadline = state.live_claim(node, token, now)?;
        state.expirations.remove(&(deadline, node));
        let renewed = now + LEASE;
        state.claims.insert(node, (token, renewed));
        state.expirations.insert((renewed, node));
        Ok(())
    }

    async fn complete(&self, completion: Completion) -> Result<(), Error> {
        let node = Node::try_from(completion.range)?;
        if completion.text.trim().is_empty() || completion.text.len() > MAX_SUMMARY_BYTES {
            return Err(Error::Invalid);
        }
        let mut state = self.state.lock().await;
        if let Some(published) = state.summaries.get(&node) {
            return if *published.text == *completion.text
                && published.token == Some(completion.token)
            {
                Ok(())
            } else {
                Err(Error::ClaimLost)
            };
        }
        state.live_claim(node, completion.token, Instant::now())?;
        let summary = september_memory::Summary::new(node, completion.text);
        let prepared = state.prepare_publication(summary, Some(completion.token))?;
        state.publish(prepared);
        Ok(())
    }
}

impl State {
    fn live_claim(&self, node: Node, token: Uuid, now: Instant) -> Result<Instant, Error> {
        let &(current, deadline) = self.claims.get(&node).ok_or(Error::ClaimLost)?;
        if current != token || deadline <= now {
            return Err(Error::ClaimLost);
        }
        Ok(deadline)
    }

    fn message(&self, id: u64) -> Result<&Message, Error> {
        usize::try_from(id)
            .ok()
            .and_then(|id| self.messages.get(id))
            .ok_or_else(|| Error::internal("retrieve original", Invariant::MissingMessage(id)))
    }

    fn summary(&self, node: Node) -> Result<Summary, Error> {
        let summary = self
            .summaries
            .get(&node)
            .ok_or_else(|| Error::internal("retrieve summary", Invariant::MissingSummary(node)))?;
        Ok(Summary {
            range: node.into(),
            text: summary.text.to_string(),
        })
    }

    /// The job's frozen context, or a new one if it fits; `None` means wait.
    fn context(&self, node: Node) -> Result<Option<Context>, Error> {
        if let Some(context) = self.contexts.get(&node) {
            return Ok(Some(context.clone()));
        }
        let prefix = self
            .views
            .context(node)
            .map_err(|error| Error::internal("select job context", error))?;
        prefix
            .map(|prefix| {
                Ok(Context {
                    cutoff: prefix.cutoff(),
                    view: prefix
                        .render()
                        .map_err(|error| Error::internal("render job context", error))?,
                })
            })
            .transpose()
    }

    fn prepare_publication(
        &self,
        summary: september_memory::Summary,
        token: Option<Uuid>,
    ) -> Result<Prepared, Error> {
        let publication = Publication::new(summary, |node| built(&self.summaries, node));
        let built = |node| {
            publication
                .text(node)
                .or_else(|| built(&self.summaries, node))
        };
        let mut views = self.views.clone();
        let reached = views
            .advance(built, |cutoff| self.snapshots.is_waiting(cutoff))
            .map_err(|error| Error::internal("advance views", error))?;
        let frozen = self.snapshots.freeze_waiting(&reached, built)?;
        Ok(Prepared {
            publication,
            token,
            views,
            frozen,
        })
    }

    fn publish(&mut self, prepared: Prepared) {
        let Prepared {
            publication,
            token,
            views,
            frozen,
        } = prepared;
        // Only the supplied summary has a claim; joined parents are verbatim.
        let tokens = std::iter::once(token).chain(std::iter::repeat(None));
        for (summary, token) in publication.summaries().iter().zip(tokens) {
            let node = summary.node();
            let text = summary.shared_text();
            self.summaries.insert(node, Published { text, token });
            self.contexts.remove(&node);
            if self.unbuilt.remove(&node)
                && let Some(&eligible) = self.unbuilt.iter().nth(MAX_ELIGIBLE_LEAVES - 1)
            {
                self.ready.push_back(eligible);
            }
            if let Some((_, deadline)) = self.claims.remove(&node) {
                self.expirations.remove(&(deadline, node));
            }
        }
        self.views = views;
        self.snapshots.make_ready(frozen);
        self.ready.extend(publication.job());
    }
}

/// Published text, or `None` for a range not yet built.
fn built(summaries: &BTreeMap<Node, Published>, node: Node) -> Option<Arc<str>> {
    summaries
        .get(&node)
        .map(|published| Arc::clone(&published.text))
}

#[cfg(test)]
mod tests;

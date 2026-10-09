use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use september_memory::{Budget, Node, Zoom};
use tokio::{sync::Mutex, time::Instant};
use uuid::Uuid;

use crate::{
    Error,
    archive::{Message, Receipt, Source},
    error::Invariant,
    jobs::{Claim, Completion, Context, Input, Job, MAX_CLAIMS, MAX_SUMMARY_BYTES, SUMMARY_BYTES},
    snapshots::{Detail, Snapshot, Summary},
    storage::Storage,
};

mod views;
use views::{Projection, Views};

const MAX_MESSAGES: usize = 1024;
const LEASE: Duration = Duration::from_secs(60);
const MAX_ELIGIBLE_LEAVES: usize = 8;

/// Volatile single-process storage, bounded to 1,024 messages and 128 snapshots.
/// Cloning an `Arc<InMemory>` shares the archive; constructing another starts empty.
pub struct InMemory {
    state: Mutex<State>,
}

struct State {
    messages: Vec<Message>,
    sources: BTreeMap<Source, u64>,
    summaries: BTreeMap<Node, Completed>,
    ready: VecDeque<Node>,
    unbuilt: BTreeSet<Node>,
    contexts: BTreeMap<Node, Context>,
    claims: BTreeMap<Node, (Uuid, Instant)>,
    expirations: BTreeSet<(Instant, Node)>,
    views: Views,
}

struct Completed {
    text: Arc<str>,
    /// The claim that published a model summary; verbatim summaries have none.
    token: Option<Uuid>,
}

/// Every change one publication makes, prepared before any of them is applied.
struct Publication {
    summaries: BTreeMap<Node, Completed>,
    projection: Projection,
    job: Option<Node>,
}

impl InMemory {
    /// Start an empty archive using the given view budget.
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
                views: Views::new(budget),
            }),
        }
    }
}

impl Default for InMemory {
    fn default() -> Self {
        Self::new(Budget::CHAT)
    }
}

impl Storage for InMemory {
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
        let publication = verbatim
            .map(|text| state.prepare_publication(node, text.into(), None))
            .transpose()?;
        // Everything that can fail has been prepared before this commit.
        state.sources.insert(message.source.clone(), id);
        state.messages.push(message);
        if let Some(publication) = publication {
            state.publish(publication);
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

    async fn prepare(&self, id: Uuid) -> Result<Snapshot, Error> {
        let mut state = self.state.lock().await;
        let cutoff = u64::try_from(state.messages.len())
            .map_err(|error| Error::internal("assign archive cutoff", error))?;
        state.views.prepare(id, cutoff)
    }

    async fn snapshot(&self, id: Uuid) -> Result<Snapshot, Error> {
        self.state.lock().await.views.get(id)
    }

    async fn zoom(&self, id: Uuid, node: Node) -> Result<Detail, Error> {
        let state = self.state.lock().await;
        match state.views.frozen(id)?.zoom(node)? {
            Zoom::Message(id) => Ok(Detail::Message {
                id,
                message: state.message(id)?.clone(),
            }),
            Zoom::Children([left, right]) => Ok(Detail::Children {
                summaries: [state.summary(left)?, state.summary(right)?],
            }),
        }
    }

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
            let context = state
                .contexts
                .get(&node)
                .cloned()
                .map_or_else(|| state.views.context(node), Ok);
            match context {
                Ok(context) => {
                    selected = Some((index, node, context));
                    break;
                }
                // Let other ready parents shrink the context before admitting this job.
                Err(Error::NotReady) => {}
                Err(error) => return Err(error),
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
        if let Some(completed) = state.summaries.get(&node) {
            return if *completed.text == *completion.text
                && completed.token == Some(completion.token)
            {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        state.live_claim(node, completion.token, Instant::now())?;
        let publication =
            state.prepare_publication(node, completion.text.into(), Some(completion.token))?;
        state.publish(publication);
        Ok(())
    }
}

impl State {
    fn live_claim(&self, node: Node, token: Uuid, now: Instant) -> Result<Instant, Error> {
        let &(current, deadline) = self.claims.get(&node).ok_or(Error::Conflict)?;
        if current != token || deadline <= now {
            return Err(Error::Conflict);
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

    /// Prepare a publication and every parent it completes verbatim. Two children
    /// that fit in one summary together are joined by a newline, with no model
    /// call; the first parent that does not fit becomes a job.
    fn prepare_publication(
        &self,
        node: Node,
        text: Arc<str>,
        token: Option<Uuid>,
    ) -> Result<Publication, Error> {
        let mut summaries = BTreeMap::from([(node, Completed { text, token })]);
        let mut job = None;
        let mut child = node;
        while let Ok(parent) = child.parent()
            && let Some([left, right]) = parent.children()
        {
            let text = |node| completed_text(&summaries, &self.summaries, node);
            // Each child publishes once, so only the second one reaches its parent.
            let (Some(left), Some(right)) = (text(left), text(right)) else {
                break;
            };
            let joined = format!("{left}\n{right}");
            if joined.len() > SUMMARY_BYTES {
                job = Some(parent);
                break;
            }
            let completed = Completed {
                text: joined.into(),
                token: None,
            };
            summaries.insert(parent, completed);
            child = parent;
        }
        let projection = self
            .views
            .advance(|node| completed_text(&summaries, &self.summaries, node))?;
        Ok(Publication {
            summaries,
            projection,
            job,
        })
    }

    fn publish(&mut self, publication: Publication) {
        let Publication {
            summaries,
            projection,
            job,
        } = publication;
        for (node, completed) in summaries {
            self.summaries.insert(node, completed);
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
        self.views.apply(projection);
        self.ready.extend(job);
    }
}

/// Completed text from a prepared publication, else from the archive.
fn completed_text(
    prepared: &BTreeMap<Node, Completed>,
    archive: &BTreeMap<Node, Completed>,
    node: Node,
) -> Option<Arc<str>> {
    prepared
        .get(&node)
        .or_else(|| archive.get(&node))
        .map(|completed| Arc::clone(&completed.text))
}

#[cfg(test)]
mod tests;

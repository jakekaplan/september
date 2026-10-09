use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use september_memory::{Budget, Node, Zoom};
use tokio::{sync::Mutex, time::Instant};
use uuid::Uuid;

use crate::{
    Error,
    archive::{Message, Receipt, Source},
    error::Invariant,
    jobs::{Claim, Completion, Context, Input},
    snapshots::{Detail, Snapshot, Summary},
    storage::Storage,
};

mod views;
use views::{Projection, Views};

const MAX_MESSAGES: usize = 1024;
const LEASE: Duration = Duration::from_secs(60);
const MAX_CLAIMS: usize = 8;
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
    delayed: BTreeSet<(Instant, Node)>,
    views: Views,
}

struct Completed {
    text: String,
    token: Option<Uuid>,
}

struct Publication {
    node: Node,
    completed: Completed,
    projection: Projection,
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
                delayed: BTreeSet::new(),
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
        let verbatim = message.verbatim_summary()?;
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
            .map(|text| state.prepare_publication(node, text, None))
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
        while let Some(&(ready_at, node)) = state.delayed.first() {
            if ready_at > now {
                break;
            }
            state.delayed.pop_first();
            state.ready.push_back(node);
        }
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
            range: node.into(),
            token,
            lease_seconds: LEASE.as_secs(),
            input,
            context,
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

    async fn release(&self, node: Node, token: Uuid, retry_after: Duration) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let deadline = state.live_claim(node, token, now)?;
        let ready_at = now.checked_add(retry_after).ok_or(Error::Invalid)?;
        state.claims.remove(&node);
        state.expirations.remove(&(deadline, node));
        if retry_after.is_zero() {
            state.ready.push_back(node);
        } else {
            state.delayed.insert((ready_at, node));
        }
        Ok(())
    }

    async fn complete(&self, completion: Completion) -> Result<(), Error> {
        let node = Node::try_from(completion.range)?;
        if completion.text.trim().is_empty() || completion.text.len() > 512 {
            return Err(Error::Invalid);
        }
        let mut state = self.state.lock().await;
        if let Some(completed) = state.summaries.get(&node) {
            return if completed.text == completion.text && completed.token == Some(completion.token)
            {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        state.live_claim(node, completion.token, Instant::now())?;
        let publication =
            state.prepare_publication(node, completion.text, Some(completion.token))?;
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
            text: summary.text.clone(),
        })
    }

    fn prepare_publication(
        &self,
        node: Node,
        text: String,
        token: Option<Uuid>,
    ) -> Result<Publication, Error> {
        let projection = self.views.advance(&self.summaries, node, &text)?;
        Ok(Publication {
            node,
            completed: Completed { text, token },
            projection,
        })
    }

    fn publish(&mut self, publication: Publication) {
        let Publication {
            node,
            completed,
            projection,
        } = publication;
        self.summaries.insert(node, completed);
        self.views.apply(projection);
        self.contexts.remove(&node);
        if self.unbuilt.remove(&node)
            && let Some(&eligible) = self.unbuilt.iter().nth(MAX_ELIGIBLE_LEAVES - 1)
        {
            self.ready.push_back(eligible);
        }
        if let Some((_, deadline)) = self.claims.remove(&node) {
            self.expirations.remove(&(deadline, node));
        }
        // Each child publishes once. Exactly the second publication enqueues its parent.
        if let Ok(parent) = node.parent()
            && let Some(children) = parent.children()
            && children
                .iter()
                .all(|child| self.summaries.contains_key(child))
        {
            self.ready.push_back(parent);
        }
    }
}

#[cfg(test)]
mod tests;

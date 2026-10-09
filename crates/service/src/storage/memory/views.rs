use std::collections::BTreeMap;

use september_memory::{Budget, Node, Snapshot as Frozen, Summary, View};
use uuid::Uuid;

use crate::{Error, jobs::Context, snapshots::Snapshot};

use super::Completed;

pub(super) struct Views {
    live: View,
    compaction: View,
    snapshots: BTreeMap<Uuid, Saved>,
    pending: BTreeMap<u64, Vec<Uuid>>,
}

struct Saved {
    cutoff: u64,
    frozen: Option<Frozen>,
}

pub(super) struct Projection {
    live: View,
    compaction: View,
    ready: Vec<(u64, Frozen)>,
}

impl Views {
    pub(super) fn new(budget: Budget) -> Self {
        Self {
            live: View::new(budget),
            compaction: View::new(Budget::COMPACTION),
            snapshots: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }

    pub(super) fn prepare(&mut self, id: Uuid, cutoff: u64) -> Result<Snapshot, Error> {
        if self.snapshots.contains_key(&id) {
            return self.get(id);
        }
        if self.snapshots.len() >= 128 {
            return Err(Error::Capacity);
        }
        let frozen = if self.live.cutoff() == cutoff {
            Some(
                self.live
                    .freeze(cutoff)
                    .map_err(|error| Error::internal("freeze snapshot", error))?,
            )
        } else {
            None
        };
        if frozen.is_none() {
            self.pending.entry(cutoff).or_default().push(id);
        }
        self.snapshots.insert(id, Saved { cutoff, frozen });
        self.get(id)
    }

    pub(super) fn get(&self, id: Uuid) -> Result<Snapshot, Error> {
        let saved = self.snapshots.get(&id).ok_or(Error::NotFound)?;
        Ok(match &saved.frozen {
            None => Snapshot::Pending {
                id,
                cutoff: saved.cutoff,
            },
            Some(frozen) => Snapshot::Ready {
                id,
                cutoff: saved.cutoff,
                nodes: frozen.nodes().iter().copied().map(Into::into).collect(),
                view: frozen.render().to_owned(),
            },
        })
    }

    pub(super) fn frozen(&self, id: Uuid) -> Result<&Frozen, Error> {
        self.snapshots
            .get(&id)
            .ok_or(Error::NotFound)?
            .frozen
            .as_ref()
            .ok_or(Error::NotReady)
    }

    /// Prepare view changes against one prospective publication, without mutating storage.
    pub(super) fn advance(
        &self,
        summaries: &BTreeMap<Node, Completed>,
        node: Node,
        text: &str,
    ) -> Result<Projection, Error> {
        let lookup = |range: Node| -> Option<&str> {
            if range == node {
                Some(text)
            } else {
                summaries.get(&range).map(|summary| summary.text.as_str())
            }
        };
        let mut live = self.live.clone();
        let mut compaction = self.compaction.clone();
        let mut ready = Vec::new();
        let mut parents = BTreeMap::new();
        // Load ancestors of the actual cover, including parents enabled later in a batch.
        for summary in live.summaries().iter().chain(compaction.summaries()) {
            add_parents(summary.node(), &lookup, &mut parents);
        }
        let previous_lines = live.summaries().len();
        live.compact(&parents)
            .map_err(|error| Error::internal("compact live view", error))?;
        if live.summaries().len() < previous_lines {
            compaction = live.clone();
            compaction.resize(Budget::COMPACTION, &parents)?;
        } else {
            compaction.compact(&parents)?;
        }
        loop {
            if self.pending.contains_key(&live.cutoff()) {
                ready.push((
                    live.cutoff(),
                    live.freeze(live.cutoff())
                        .map_err(|error| Error::internal("freeze pending snapshot", error))?,
                ));
            }
            let leaf = Node::new(live.cutoff(), 1)?;
            let Some(text) = lookup(leaf) else { break };
            add_parents(leaf, &lookup, &mut parents);
            let previous_lines = live.summaries().len();
            live.append(Summary::new(leaf, text), &parents)
                .map_err(|error| Error::internal("advance live view", error))?;
            if live.summaries().len() <= previous_lines {
                compaction = live.clone();
                compaction.resize(Budget::COMPACTION, &parents)?;
            } else {
                compaction.append(Summary::new(leaf, text), &parents)?;
            }
        }
        Ok(Projection {
            live,
            compaction,
            ready,
        })
    }

    pub(super) fn context(&self, node: Node) -> Result<Context, Error> {
        let boundary = if node.length() == 1 {
            node.start()
        } else {
            node.end()
        };
        let cutoff = boundary.min(self.compaction.cutoff());
        // No completed ancestor can cross an unbuilt job's boundary.
        // Publication already compacts the cover; a prefix enables no new merges.
        let prefix = self.compaction.prefix(cutoff)?;
        if prefix.rendered_bytes()? > Budget::COMPACTION.trigger() {
            return Err(Error::NotReady);
        }
        Ok(Context {
            cutoff,
            view: prefix.render()?,
        })
    }

    pub(super) fn apply(&mut self, projection: Projection) {
        self.live = projection.live;
        self.compaction = projection.compaction;
        for (cutoff, frozen) in projection.ready {
            if let Some(ids) = self.pending.remove(&cutoff) {
                for id in ids {
                    if let Some(saved) = self.snapshots.get_mut(&id) {
                        saved.frozen = Some(frozen.clone());
                    }
                }
            }
        }
    }
}

fn add_parents<'a>(
    mut node: Node,
    lookup: &impl Fn(Node) -> Option<&'a str>,
    parents: &mut BTreeMap<Node, String>,
) {
    while let Ok(parent) = node.parent() {
        // A missing parent cannot have a completed ancestor: publication enforces children.
        let Some(text) = lookup(parent) else { break };
        if parents.contains_key(&parent) {
            break;
        }
        parents.insert(parent, text.to_owned());
        node = parent;
    }
}

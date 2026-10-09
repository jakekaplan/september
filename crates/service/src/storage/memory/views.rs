use std::collections::BTreeMap;
use std::sync::Arc;

use september_memory::{Budget, Node, Snapshot as Frozen, Summary, View};
use uuid::Uuid;

use crate::{Error, jobs::Context, snapshots::Snapshot};

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

    /// Prepare view changes for a publication, without mutating storage.
    /// `built` must include the publication's own summaries.
    pub(super) fn advance(
        &self,
        built: impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<Projection, Error> {
        self.project(&built)
            .map_err(|error| Error::internal("advance views", error))
    }

    fn project(
        &self,
        built: &impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<Projection, september_memory::Error> {
        let mut live = self.live.clone();
        let mut compaction = self.compaction.clone();
        let mut ready = Vec::new();
        // A new parent can resume an unfinished batch before any leaf arrives.
        let merges = live.compact(built)?;
        follow(&mut compaction, &live, merges, None, built)?;
        loop {
            if self.pending.contains_key(&live.cutoff()) {
                ready.push((live.cutoff(), live.freeze(live.cutoff())?));
            }
            let leaf = Node::new(live.cutoff(), 1)?;
            let Some(text) = built(leaf) else { break };
            let leaf = Summary::new(leaf, text);
            let merges = live.append(leaf.clone(), built)?;
            follow(&mut compaction, &live, merges, Some(leaf), built)?;
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

/// Keep the compaction view on the live view's lines. After a live merge it is
/// derived again and batched to its own target; otherwise it takes the same
/// new line, or resumes its own unfinished batch.
fn follow(
    compaction: &mut View,
    live: &View,
    live_merges: usize,
    leaf: Option<Summary>,
    built: &impl Fn(Node) -> Option<Arc<str>>,
) -> Result<(), september_memory::Error> {
    if live_merges > 0 {
        *compaction = live.clone();
        compaction.resize(Budget::COMPACTION, built)?;
    } else if let Some(leaf) = leaf {
        compaction.append(leaf, built)?;
    } else {
        compaction.compact(built)?;
    }
    Ok(())
}

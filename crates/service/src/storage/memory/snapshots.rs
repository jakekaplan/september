use std::collections::BTreeMap;
use std::sync::Arc;

use september_memory::{Budget, Node, Snapshot as Frozen, View};
use uuid::Uuid;

use crate::{
    Error,
    snapshots::{Snapshot, freeze},
};

const MAX_SNAPSHOTS: usize = 128;

/// Interactions by ID, each frozen now or waiting for its exact cutoff.
#[derive(Default)]
pub(super) struct Snapshots {
    saved: BTreeMap<Uuid, Saved>,
    waiting: BTreeMap<u64, Vec<Uuid>>,
}

struct Saved {
    cutoff: u64,
    within: Option<Budget>,
    frozen: Option<Frozen>,
}

impl Snapshots {
    /// Freeze `live` for a new interaction if it covers `cutoff`, else wait for it.
    /// A retried ID returns its original snapshot.
    pub(super) fn prepare(
        &mut self,
        id: Uuid,
        cutoff: u64,
        within: Option<Budget>,
        live: &View,
        built: impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<Snapshot, Error> {
        if self.saved.contains_key(&id) {
            return self.get(id);
        }
        if self.saved.len() >= MAX_SNAPSHOTS {
            return Err(Error::Capacity);
        }
        let frozen = if live.cutoff() == cutoff {
            Some(freeze(live, cutoff, within, built)?)
        } else {
            self.waiting.entry(cutoff).or_default().push(id);
            None
        };
        self.saved.insert(
            id,
            Saved {
                cutoff,
                within,
                frozen,
            },
        );
        self.get(id)
    }

    pub(super) fn get(&self, id: Uuid) -> Result<Snapshot, Error> {
        let saved = self.saved.get(&id).ok_or(Error::NotFound)?;
        Ok(Snapshot::new(id, saved.cutoff, saved.frozen.as_ref()))
    }

    pub(super) fn frozen(&self, id: Uuid) -> Result<&Frozen, Error> {
        self.saved
            .get(&id)
            .ok_or(Error::NotFound)?
            .frozen
            .as_ref()
            .ok_or(Error::NotReady)
    }

    /// Whether any interaction waits for the live view to reach `cutoff`.
    pub(super) fn is_waiting(&self, cutoff: u64) -> bool {
        self.waiting.contains_key(&cutoff)
    }

    /// Freeze each interaction waiting at the cutoffs the live view `reached`,
    /// at its own size, without changing anything yet.
    pub(super) fn freeze_waiting(
        &self,
        reached: &[View],
        built: impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<Vec<(Uuid, Frozen)>, Error> {
        let mut frozen = Vec::new();
        for view in reached {
            let waiting = self.waiting.get(&view.cutoff()).into_iter().flatten();
            for (&id, saved) in waiting.filter_map(|id| Some((id, self.saved.get(id)?))) {
                frozen.push((id, freeze(view, saved.cutoff, saved.within, &built)?));
            }
        }
        Ok(frozen)
    }

    /// Give waiting interactions the snapshots frozen for them.
    pub(super) fn make_ready(&mut self, frozen: Vec<(Uuid, Frozen)>) {
        for (id, snapshot) in frozen {
            self.waiting.remove(&snapshot.cutoff());
            if let Some(saved) = self.saved.get_mut(&id) {
                saved.frozen = Some(snapshot);
            }
        }
    }
}

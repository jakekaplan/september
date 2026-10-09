use std::collections::BTreeMap;

use september_memory::{Snapshot as Frozen, View};
use uuid::Uuid;

use crate::{Error, snapshots::Snapshot};

const MAX_SNAPSHOTS: usize = 128;

/// Interactions by ID, each frozen now or waiting for its exact cutoff.
#[derive(Default)]
pub(super) struct Snapshots {
    saved: BTreeMap<Uuid, Saved>,
    waiting: BTreeMap<u64, Vec<Uuid>>,
}

struct Saved {
    cutoff: u64,
    frozen: Option<Frozen>,
}

impl Snapshots {
    /// Freeze `live` for a new interaction if it covers `cutoff`, else wait for it.
    /// A retried ID returns its original snapshot.
    pub(super) fn prepare(
        &mut self,
        id: Uuid,
        cutoff: u64,
        live: &View,
    ) -> Result<Snapshot, Error> {
        if self.saved.contains_key(&id) {
            return self.get(id);
        }
        if self.saved.len() >= MAX_SNAPSHOTS {
            return Err(Error::Capacity);
        }
        let frozen = if live.cutoff() == cutoff {
            Some(
                live.freeze(cutoff)
                    .map_err(|error| Error::internal("freeze snapshot", error))?,
            )
        } else {
            self.waiting.entry(cutoff).or_default().push(id);
            None
        };
        self.saved.insert(id, Saved { cutoff, frozen });
        self.get(id)
    }

    pub(super) fn get(&self, id: Uuid) -> Result<Snapshot, Error> {
        let saved = self.saved.get(&id).ok_or(Error::NotFound)?;
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

    /// Give each waiting interaction the snapshot frozen at its cutoff.
    pub(super) fn make_ready(&mut self, frozen: Vec<Frozen>) {
        for snapshot in frozen {
            for id in self.waiting.remove(&snapshot.cutoff()).unwrap_or_default() {
                if let Some(saved) = self.saved.get_mut(&id) {
                    saved.frozen = Some(snapshot.clone());
                }
            }
        }
    }
}

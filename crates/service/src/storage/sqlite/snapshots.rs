use std::{collections::BTreeSet, sync::Arc};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use september_memory::{Budget, Node, Snapshot as Frozen, View};
use uuid::Uuid;

use super::{Lookup, State, cover, decode_nodes, encode_nodes, load_view, next_id};
use crate::{
    Error,
    snapshots::{Snapshot, freeze},
};

impl State {
    /// Freeze the live view for a new interaction if it covers the archive, else
    /// save the interaction to wait for it. A retried ID returns its snapshot.
    pub(super) fn prepare(&mut self, id: Uuid, within: Option<Budget>) -> Result<Snapshot, Error> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        match saved(&tx, id) {
            Ok((cutoff, frozen)) => return Ok(Snapshot::new(id, cutoff, frozen.as_ref())),
            Err(Error::NotFound) => {}
            Err(error) => return Err(error),
        }
        let cutoff = next_id(&tx)?;
        let live = load_view(&tx, "live", self.budget)?;
        let frozen = if live.cutoff() == cutoff {
            let lookup = Lookup::new(&tx);
            let frozen = freeze(&live, cutoff, within, |node| lookup.text(node))?;
            lookup.check()?;
            Some(frozen)
        } else {
            None
        };
        let nodes = frozen
            .as_ref()
            .map(|frozen| encode_nodes(frozen.nodes()))
            .transpose()?;
        tx.prepare_cached(
            "INSERT INTO snapshots (id, cutoff, within, nodes) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            id.to_string(),
            cutoff,
            within.map(Budget::target),
            nodes
        ])?;
        tx.commit()?;
        Ok(Snapshot::new(id, cutoff, frozen.as_ref()))
    }
}

/// The cutoffs interactions are waiting for the live view to reach.
pub(super) fn waiting(db: &Connection) -> Result<BTreeSet<u64>, Error> {
    Ok(db
        .prepare_cached("SELECT DISTINCT cutoff FROM snapshots WHERE nodes IS NULL")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?)
}

/// Freeze each interaction waiting at the cutoffs the live view `reached`, at
/// its own size, without saving anything yet.
pub(super) fn freeze_waiting(
    db: &Connection,
    reached: &[View],
    built: impl Fn(Node) -> Option<Arc<str>> + Copy,
) -> Result<Vec<(String, Frozen)>, Error> {
    let mut frozen = Vec::new();
    for view in reached {
        let waiting: Vec<(String, Option<usize>)> = db
            .prepare_cached("SELECT id, within FROM snapshots WHERE cutoff = ?1 AND nodes IS NULL")?
            .query_map([view.cutoff()], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (id, within) in waiting {
            let within = within
                .map(Budget::at_most)
                .transpose()
                .map_err(|error| Error::internal("restore snapshot size", error))?;
            frozen.push((id, freeze(view, view.cutoff(), within, built)?));
        }
    }
    Ok(frozen)
}

/// Save the covers frozen for waiting interactions.
pub(super) fn make_ready(db: &Connection, frozen: &[(String, Frozen)]) -> Result<(), Error> {
    for (id, snapshot) in frozen {
        db.prepare_cached("UPDATE snapshots SET nodes = ?2 WHERE id = ?1")?
            .execute(params![id, encode_nodes(snapshot.nodes())?])?;
    }
    Ok(())
}

/// A saved snapshot's cutoff, and its frozen cover once ready.
pub(super) fn saved(db: &Connection, id: Uuid) -> Result<(u64, Option<Frozen>), Error> {
    let (cutoff, nodes): (u64, Option<String>) = db
        .prepare_cached("SELECT cutoff, nodes FROM snapshots WHERE id = ?1")?
        .query_row([id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?
        .ok_or(Error::NotFound)?;
    let frozen = nodes
        .map(|nodes| {
            let cover = cover(db, &decode_nodes(&nodes)?)?;
            Frozen::restore(&cover).map_err(|error| Error::internal("restore snapshot", error))
        })
        .transpose()?;
    Ok((cutoff, frozen))
}

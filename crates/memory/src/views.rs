use std::sync::Arc;

use crate::{Budget, Error, Node, Summary, View};

/// The live view and the smaller compaction view that gives summary jobs context.
///
/// As in the gist, the compaction view is the live view merged further, with its
/// own sawtooth. It is derived again and batched to its target whenever the live
/// view merges; otherwise it takes the same new lines. Save both together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Views {
    live: View,
    compaction: View,
}

impl Views {
    /// Start empty views with the live and compaction budgets.
    #[must_use]
    pub fn new(live: Budget, compaction: Budget) -> Self {
        Self {
            live: View::new(live),
            compaction: View::new(compaction),
        }
    }

    /// Restore views saved together, without selecting new summaries.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidView`] if they cover different prefixes.
    pub fn restore(live: View, compaction: View) -> Result<Self, Error> {
        if live.cutoff() != compaction.cutoff() {
            return Err(Error::InvalidView);
        }
        Ok(Self { live, compaction })
    }

    /// The view interactions freeze.
    #[must_use]
    pub const fn live(&self) -> &View {
        &self.live
    }

    /// The smaller view summary jobs take their context from.
    #[must_use]
    pub const fn compaction(&self) -> &View {
        &self.compaction
    }

    /// Append every built leaf after the live cutoff, advancing both batches.
    ///
    /// `built` returns completed text, or `None` for a range not yet built, as for
    /// [`View::compact`]. Returns a copy of the live view at each cutoff it reaches
    /// that `wanted` asks for, including its starting one, for freezing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] for an unrepresentable range or byte count.
    /// Failure leaves both views unchanged.
    pub fn advance(
        &mut self,
        built: impl Fn(Node) -> Option<Arc<str>>,
        wanted: impl Fn(u64) -> bool,
    ) -> Result<Vec<View>, Error> {
        let mut next = self.clone();
        let reached = next.extend(&built, &wanted)?;
        *self = next;
        Ok(reached)
    }

    /// The compaction-view prefix that gives context to building `node`.
    ///
    /// It holds the lines before a leaf, or through a parent's children, and
    /// stops early at the first unbuilt leaf. An unbuilt node has no completed
    /// ancestor, so the boundary never splits a line.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] for an unrepresentable byte count. Returns
    /// `Ok(None)` while the prefix exceeds the compaction trigger; building other
    /// parents can shrink it.
    pub fn context(&self, node: Node) -> Result<Option<View>, Error> {
        let boundary = if node.length() == 1 {
            node.start()
        } else {
            node.end()
        };
        let prefix = self
            .compaction
            .prefix(boundary.min(self.compaction.cutoff()))?;
        let fits = prefix.rendered_bytes()? <= self.compaction.budget().trigger();
        Ok(fits.then_some(prefix))
    }

    fn extend(
        &mut self,
        built: &impl Fn(Node) -> Option<Arc<str>>,
        wanted: &impl Fn(u64) -> bool,
    ) -> Result<Vec<View>, Error> {
        // A new parent can resume an unfinished batch before any leaf arrives.
        let merges = self.live.compact(built)?;
        self.follow(merges, None, built)?;
        let mut reached = Vec::new();
        loop {
            let cutoff = self.live.cutoff();
            if wanted(cutoff) {
                reached.push(self.live.clone());
            }
            let leaf = Node::new(cutoff, 1)?;
            let Some(text) = built(leaf) else { break };
            let leaf = Summary::new(leaf, text);
            let merges = self.live.append(leaf.clone(), built)?;
            self.follow(merges, Some(leaf), built)?;
        }
        Ok(reached)
    }

    fn follow(
        &mut self,
        live_merges: usize,
        leaf: Option<Summary>,
        built: &impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<(), Error> {
        if live_merges > 0 {
            let budget = self.compaction.budget();
            self.compaction = self.live.clone();
            self.compaction.resize(budget, built)?;
        } else if let Some(leaf) = leaf {
            self.compaction.append(leaf, built)?;
        } else {
            self.compaction.compact(built)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

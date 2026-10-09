use std::sync::Arc;

use crate::{Error, Node, Summary};

const OPEN: &str = "<chat>\n";
const CLOSE: &str = "</chat>";
const EMPTY_BYTES: usize = OPEN.len() + CLOSE.len();

/// UTF-8 byte limits for a persisted, batched view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    target: usize,
    trigger: usize,
}

impl Budget {
    /// The main conversation view's 64,000–128,000-byte sawtooth.
    pub const CHAT: Self = Self {
        target: 64_000,
        trigger: 128_000,
    };
    /// The worker context view's 16,000–32,000-byte sawtooth.
    pub const COMPACTION: Self = Self {
        target: 16_000,
        trigger: 32_000,
    };

    /// Validate a batch target and a strictly larger trigger.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidBudget`] if the target cannot fit the empty
    /// `<chat>` rendering or the trigger is not strictly above the target.
    pub fn new(target: usize, trigger: usize) -> Result<Self, Error> {
        if target < EMPTY_BYTES || trigger <= target {
            return Err(Error::InvalidBudget { target, trigger });
        }
        Ok(Self { target, trigger })
    }

    /// Limits for a one-off view of at most `bytes`, for a harness that takes
    /// less context than the live view uses.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidBudget`] if `bytes` cannot fit the empty `<chat>`
    /// rendering.
    pub fn at_most(bytes: usize) -> Result<Self, Error> {
        // A one-off view batches straight to its target; the trigger only has to
        // exceed it.
        Self::new(bytes, bytes.saturating_add(1))
    }

    /// The rendered byte size a triggered batch tries to reach.
    #[must_use]
    pub const fn target(self) -> usize {
        self.target
    }

    /// A batch starts only after the rendered view exceeds this byte size.
    #[must_use]
    pub const fn trigger(self) -> usize {
        self.trigger
    }
}

/// An owned chronological summary cover, with retained batch state.
///
/// Append completed leaves; do not reconstruct the cover from the archive. Save
/// its node identities and [`Self::is_shrinking`] together, then load only the
/// referenced completed summaries. No full-history collection is needed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct View {
    summaries: Vec<Summary>,
    shrinking: bool,
    budget: Budget,
}

impl View {
    /// Start an empty view with the supplied validated byte budget.
    #[must_use]
    pub fn new(budget: Budget) -> Self {
        Self {
            summaries: Vec::new(),
            shrinking: false,
            budget,
        }
    }

    /// Restore the saved cover and batch state without selecting new summaries.
    ///
    /// Supply only the referenced completed records and the original budget.
    /// Descendants are not required. An unfinished batch is valid above its
    /// target, including below its trigger; a normal view cannot exceed its trigger.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidView`] for gaps, overlaps, or inconsistent batch
    /// state, or [`Error::Overflow`] if the rendered size cannot be represented.
    pub fn restore(
        summaries: Vec<Summary>,
        shrinking: bool,
        budget: Budget,
    ) -> Result<Self, Error> {
        if !is_cover(&summaries) {
            return Err(Error::InvalidView);
        }
        let view = Self {
            summaries,
            shrinking,
            budget,
        };
        let bytes = view.rendered_bytes()?;
        if (shrinking && bytes <= budget.target) || (!shrinking && bytes > budget.trigger) {
            return Err(Error::InvalidView);
        }
        Ok(view)
    }

    /// The chronological completed summaries to inspect or save by node identity.
    #[must_use]
    pub fn summaries(&self) -> &[Summary] {
        &self.summaries
    }

    /// The exclusive archive cutoff covered by this view.
    #[must_use]
    pub fn cutoff(&self) -> u64 {
        self.summaries
            .last()
            .map_or(0, |summary| summary.node().end())
    }

    /// The byte limits this view batches within.
    #[must_use]
    pub const fn budget(&self) -> Budget {
        self.budget
    }

    /// Whether a triggered batch still needs to reach its byte target.
    #[must_use]
    pub const fn is_shrinking(&self) -> bool {
        self.shrinking
    }

    /// Append the next completed leaf and advance any required batch.
    ///
    /// Below the trigger and outside a pending batch, only the final line is
    /// added. Look up built parents as described by [`Self::compact`].
    ///
    /// Returns the number of merges, so callers can tell whether earlier lines
    /// changed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidLeaf`] for a parent, [`Error::InvalidView`] for an
    /// out-of-order ID, or [`Error::Overflow`] for an unrepresentable byte count.
    /// Failure leaves this view unchanged.
    pub fn append(
        &mut self,
        leaf: Summary,
        built: impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<usize, Error> {
        if leaf.node().length() != 1 {
            return Err(Error::InvalidLeaf(leaf.node()));
        }
        if leaf.node().start() != self.cutoff() {
            return Err(Error::InvalidView);
        }
        let mut candidate = self.clone();
        candidate.summaries.push(leaf);
        self.commit(candidate, &built)
    }

    /// Advance a triggered or unfinished batch, returning the number of merges.
    ///
    /// `built` returns a parent's completed text, or `None` while that parent is
    /// unbuilt. It must answer for every range, including parents enabled by
    /// earlier merges in this batch. Publication must already enforce immutable
    /// text and completed children; this operation does not load descendants.
    ///
    /// Rank by `(cutoff - pair_last_message) / child_length`, comparing exact
    /// integers and choosing the oldest pair on ties. Stop at the target or when
    /// no built parent remains. A blocked batch retains its shrinking state even
    /// below the trigger, and can resume when more parents are built.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] for an unrepresentable rendered size. Failure
    /// leaves this view unchanged. A blocked batch is not an error.
    pub fn compact(&mut self, built: impl Fn(Node) -> Option<Arc<str>>) -> Result<usize, Error> {
        self.commit(self.clone(), &built)
    }

    /// Select a historical prefix ending at an existing cover boundary.
    /// No summary is split or replaced with descendants.
    ///
    /// # Errors
    /// Returns [`Error::CutoffMismatch`] if the cutoff crosses a cover node or
    /// exceeds this view, or [`Error::Overflow`] for an unrepresentable size.
    pub fn prefix(&self, cutoff: u64) -> Result<Self, Error> {
        let summaries = self
            .summaries
            .iter()
            .take_while(|summary| summary.node().end() <= cutoff)
            .cloned()
            .collect();
        let mut prefix = Self {
            summaries,
            shrinking: self.shrinking,
            budget: self.budget,
        };
        if prefix.cutoff() != cutoff {
            return Err(Error::CutoffMismatch {
                expected: cutoff,
                actual: prefix.cutoff(),
            });
        }
        prefix.shrinking &= prefix.rendered_bytes()? > prefix.budget.target;
        Ok(prefix)
    }

    /// Derive a view with another budget, immediately batching toward its target.
    /// Retain the resulting cover and unfinished batch state between updates.
    ///
    /// # Errors
    /// Returns [`Error::Overflow`] for an unrepresentable size. Failure leaves
    /// this view unchanged; unbuilt parents retain an unfinished batch.
    pub fn resize(
        &mut self,
        budget: Budget,
        built: impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<usize, Error> {
        let mut candidate = self.clone();
        candidate.budget = budget;
        candidate.shrinking = true;
        self.commit(candidate, &built)
    }

    /// Batch `candidate` and replace this view only if every step succeeds.
    fn commit(
        &mut self,
        mut candidate: Self,
        built: &impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<usize, Error> {
        let merges = candidate.batch(built)?;
        *self = candidate;
        Ok(merges)
    }

    fn batch(&mut self, built: &impl Fn(Node) -> Option<Arc<str>>) -> Result<usize, Error> {
        let mut bytes = self.rendered_bytes()?;
        if !self.shrinking && bytes <= self.budget.trigger {
            return Ok(0);
        }
        let cutoff = self.cutoff();
        let mut merges = 0;
        while bytes > self.budget.target {
            let Some((index, parent)) = most_due_pair(&self.summaries, cutoff, built) else {
                break;
            };
            let removed = self.summaries[index]
                .rendered_bytes()?
                .checked_add(self.summaries[index + 1].rendered_bytes()?)
                .ok_or(Error::Overflow)?;
            let added = parent.rendered_bytes()?;
            bytes = bytes
                .checked_sub(removed)
                .and_then(|size| size.checked_add(added))
                .ok_or(Error::Overflow)?;
            self.summaries[index] = parent;
            self.summaries.remove(index + 1);
            merges += 1;
        }
        self.shrinking = bytes > self.budget.target;
        Ok(merges)
    }

    /// Count every rendered UTF-8 byte, including range headers and `<chat>` tags.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] if the byte count cannot be represented.
    pub fn rendered_bytes(&self) -> Result<usize, Error> {
        rendered_bytes(&self.summaries)
    }

    /// Render summaries oldest first, flattening CR and LF to spaces.
    ///
    /// No dates, originals, or placeholders are added. Rendering needs only the
    /// owned cover, never an archive lookup or the ready-parent working set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] if the rendered size cannot be represented.
    pub fn render(&self) -> Result<String, Error> {
        render(&self.summaries)
    }
}

/// Whether `summaries` cover the archive prefix from 0 without gaps or overlaps.
pub(crate) fn is_cover(summaries: &[Summary]) -> bool {
    let mut next = 0;
    for summary in summaries {
        let node = summary.node();
        if node.start() != next {
            return false;
        }
        next = node.end();
    }
    true
}

fn rendered_bytes(summaries: &[Summary]) -> Result<usize, Error> {
    summaries.iter().try_fold(EMPTY_BYTES, |size, summary| {
        size.checked_add(summary.rendered_bytes()?)
            .ok_or(Error::Overflow)
    })
}

/// Render a cover inside `<chat>` tags, oldest first.
pub(crate) fn render(summaries: &[Summary]) -> Result<String, Error> {
    let mut rendered = String::with_capacity(rendered_bytes(summaries)?);
    rendered.push_str(OPEN);
    for summary in summaries {
        summary.render_into(&mut rendered);
    }
    rendered.push_str(CLOSE);
    Ok(rendered)
}

fn most_due_pair(
    summaries: &[Summary],
    cutoff: u64,
    built: &impl Fn(Node) -> Option<Arc<str>>,
) -> Option<(usize, Summary)> {
    let mut best: Option<(usize, Summary, u128, u128)> = None;
    for (index, pair) in summaries.windows(2).enumerate() {
        let [left, right] = [pair[0].node(), pair[1].node()];
        if left.length() != right.length() {
            continue;
        }
        let Ok(parent) = left.parent() else { continue };
        if parent.children() != Some([left, right]) {
            continue;
        }
        // `last` is inclusive: using the exclusive end changes cross-level rank.
        let age = u128::from(cutoff - (right.end() - 1));
        let width = u128::from(left.length());
        if best
            .as_ref()
            .is_some_and(|(_, _, best_age, best_width)| age * best_width <= best_age * width)
        {
            continue;
        }
        let Some(text) = built(parent) else { continue };
        best = Some((index, Summary::new(parent, text), age, width));
    }
    best.map(|(index, parent, _, _)| (index, parent))
}

#[cfg(test)]
mod tests;

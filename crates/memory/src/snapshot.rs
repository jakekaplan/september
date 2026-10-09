use std::sync::Arc;

use crate::{Budget, Error, Node, Summary, View};

/// A ready interaction's fixed view and permitted tree navigation.
///
/// Created through [`View::freeze`]. A request can open a frozen cover node or
/// its descendants, never a later-created ancestor spanning multiple cover lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    nodes: Vec<Node>,
    rendered: String,
}

/// The immutable range selection for a subsequent archive lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Zoom {
    /// The left and right child ranges whose completed text the service retrieves.
    Children([Node; 2]),
    /// An original-message ID whose body, timestamp, and attachments the service retrieves.
    Message(u64),
}

impl View {
    /// Freeze this complete cover at the caller's expected exclusive cutoff.
    ///
    /// Only supplied completed summaries are present. A blocked shrinking batch
    /// can still be frozen; the caller must separately enforce model-context
    /// limits. Archive identity and content lookup belong to the service.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CutoffMismatch`] for a different covered prefix, or
    /// [`Error::Overflow`] if the rendered size cannot be represented.
    pub fn freeze(&self, expected_cutoff: u64) -> Result<Snapshot, Error> {
        let actual = self.cutoff();
        if actual != expected_cutoff {
            return Err(Error::CutoffMismatch {
                expected: expected_cutoff,
                actual,
            });
        }
        Ok(Snapshot {
            nodes: self.summaries().iter().map(Summary::node).collect(),
            rendered: self.render()?,
        })
    }

    /// Freeze a copy merged toward `budget` first, leaving this view unchanged.
    ///
    /// Merges follow the usual order but use only built parents, as for
    /// [`View::compact`], so the copy can stay above `budget` while parents are
    /// unbuilt. The copy covers the same messages, just in fewer, coarser lines.
    ///
    /// # Errors
    ///
    /// As for [`View::freeze`].
    pub fn freeze_within(
        &self,
        expected_cutoff: u64,
        budget: Budget,
        built: impl Fn(Node) -> Option<Arc<str>>,
    ) -> Result<Snapshot, Error> {
        let mut copy = self.clone();
        copy.resize(budget, built)?;
        copy.freeze(expected_cutoff)
    }
}

impl Snapshot {
    /// The exclusive end derived from this interaction's frozen cover.
    #[must_use]
    pub fn cutoff(&self) -> u64 {
        self.nodes.last().map_or(0, |node| node.end())
    }

    /// The fixed chronological node cover.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The fixed `<chat>` view, independent of the live view and archive lookups.
    #[must_use]
    pub fn render(&self) -> &str {
        &self.rendered
    }

    /// Open a frozen cover node or one of its descendants.
    ///
    /// Aligned power-of-two ranges contained by a cover node are descendants.
    /// New archive records cannot expand this set. This selects ranges only;
    /// the service subsequently retrieves their immutable completed content.
    ///
    /// # Errors
    ///
    /// Returns [`Error::OutsideSnapshot`] for newer messages or an ancestor
    /// spanning multiple frozen cover lines, even when its text later completes.
    pub fn zoom(&self, node: Node) -> Result<Zoom, Error> {
        if !self
            .nodes
            .iter()
            .any(|root| root.start() <= node.start() && node.end() <= root.end())
        {
            return Err(Error::OutsideSnapshot(node));
        }
        Ok(node
            .children()
            .map_or(Zoom::Message(node.start()), Zoom::Children))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(start: u64, length: u64) -> Node {
        Node::new(start, length).unwrap()
    }

    #[test]
    fn recursive_zoom_selects_child_ranges_then_an_original_message_id() {
        let view = View::restore(
            vec![Summary::new(node(0, 4), "all four")],
            false,
            Budget::CHAT,
        )
        .unwrap();
        let snapshot = view.freeze(4).unwrap();
        assert_eq!(snapshot.cutoff(), 4);
        assert_eq!(snapshot.nodes(), &[node(0, 4)]);
        assert_eq!(snapshot.render(), "<chat>\n0+4|all four\n</chat>");
        assert_eq!(
            snapshot.zoom(node(0, 4)),
            Ok(Zoom::Children([node(0, 2), node(2, 2)]))
        );
        assert_eq!(
            snapshot.zoom(node(2, 2)),
            Ok(Zoom::Children([node(2, 1), node(3, 1)]))
        );
        assert_eq!(snapshot.zoom(node(3, 1)), Ok(Zoom::Message(3)));
    }

    #[test]
    fn rejects_both_future_ranges_and_ancestors_inside_the_cutoff() {
        let view = View::restore(
            vec![
                Summary::new(node(0, 2), "left"),
                Summary::new(node(2, 2), "right"),
            ],
            false,
            Budget::CHAT,
        )
        .unwrap();
        let snapshot = view.freeze(4).unwrap();
        for outside in [node(4, 1), node(0, 8), node(0, 4)] {
            assert_eq!(snapshot.zoom(outside), Err(Error::OutsideSnapshot(outside)));
        }
        assert_eq!(
            snapshot.zoom(node(0, 2)),
            Ok(Zoom::Children([node(0, 1), node(1, 1)]))
        );
        assert_eq!(snapshot.zoom(node(3, 1)), Ok(Zoom::Message(3)));
    }

    #[test]
    fn an_empty_snapshot_has_no_retrievable_messages() {
        let snapshot = View::new(Budget::CHAT).freeze(0).unwrap();
        assert_eq!(snapshot.cutoff(), 0);
        assert_eq!(snapshot.nodes(), []);
        assert_eq!(snapshot.render(), "<chat>\n</chat>");
        assert_eq!(
            snapshot.zoom(node(0, 1)),
            Err(Error::OutsideSnapshot(node(0, 1)))
        );
    }

    fn four_leaves() -> View {
        let mut view = View::new(Budget::CHAT);
        for id in 0..4 {
            view.append(Summary::new(node(id, 1), "0123456789"), |_| None)
                .unwrap();
        }
        view
    }

    #[test]
    fn a_sized_freeze_merges_a_copy_through_built_parents() {
        let view = four_leaves();
        let built = |node: Node| (node.length() > 1).then(|| Arc::from("p"));
        let small = view
            .freeze_within(4, Budget::at_most(20).unwrap(), built)
            .unwrap();
        assert_eq!(small.nodes(), &[node(0, 4)]);
        assert_eq!(small.render(), "<chat>\n0+4|p\n</chat>");
        // The live view keeps its own lines.
        assert_eq!(view.summaries().len(), 4);
    }

    #[test]
    fn a_sized_freeze_stays_larger_while_parents_are_unbuilt() {
        let small = four_leaves()
            .freeze_within(4, Budget::at_most(30).unwrap(), |_| None)
            .unwrap();
        assert_eq!(small.nodes().len(), 4);
        assert_eq!(
            Budget::at_most(5),
            Err(Error::InvalidBudget {
                target: 5,
                trigger: 6
            })
        );
    }

    #[test]
    fn freezing_requires_the_exact_expected_cutoff() {
        let view = View::restore(
            vec![Summary::new(node(0, 4), "complete")],
            false,
            Budget::CHAT,
        )
        .unwrap();
        for expected in [0, 3, 5] {
            assert_eq!(
                view.freeze(expected),
                Err(Error::CutoffMismatch {
                    expected,
                    actual: 4
                })
            );
        }
    }
}

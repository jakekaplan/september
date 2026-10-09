use std::sync::Arc;

use crate::{Node, SUMMARY_BYTES, Summary};

/// Everything that publishing one completed summary makes complete.
///
/// A parent whose two children fit in [`SUMMARY_BYTES`] together is their text
/// joined by a newline, with no model call. Publication climbs while that holds;
/// the first parent too long to join becomes a job. Each child publishes once,
/// so only the second child of a pair reaches its parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Publication {
    summaries: Vec<Summary>,
    job: Option<Node>,
}

impl Publication {
    /// Plan the publication of `summary`, given the archive's built summaries.
    ///
    /// `built` returns completed text, or `None` for a range not yet built.
    #[must_use]
    pub fn new(summary: Summary, built: impl Fn(Node) -> Option<Arc<str>>) -> Self {
        let mut publication = Self {
            summaries: vec![summary],
            job: None,
        };
        let mut child = publication.summaries[0].node();
        while let Ok(parent) = child.parent()
            && let Some([left, right]) = parent.children()
        {
            let text = |node| publication.text(node).or_else(|| built(node));
            let (Some(left), Some(right)) = (text(left), text(right)) else {
                break;
            };
            let joined = format!("{left}\n{right}");
            if joined.len() > SUMMARY_BYTES {
                publication.job = Some(parent);
                break;
            }
            publication.summaries.push(Summary::new(parent, joined));
            child = parent;
        }
        publication
    }

    /// The published summaries, the supplied one first, then ancestors upward.
    #[must_use]
    pub fn summaries(&self) -> &[Summary] {
        &self.summaries
    }

    /// The parent that now has both children but must be built by a model.
    #[must_use]
    pub const fn job(&self) -> Option<Node> {
        self.job
    }

    /// Completed text this publication adds, for lookups made before it commits.
    #[must_use]
    pub fn text(&self, node: Node) -> Option<Arc<str>> {
        self.summaries
            .iter()
            .find(|summary| summary.node() == node)
            .map(Summary::shared_text)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn node(start: u64, length: u64) -> Node {
        Node::new(start, length).unwrap()
    }

    fn lookup(built: &BTreeMap<Node, &str>) -> impl Fn(Node) -> Option<Arc<str>> {
        |node| built.get(&node).map(|text| Arc::from(*text))
    }

    #[test]
    fn a_first_child_publishes_alone() {
        let publication = Publication::new(Summary::new(node(0, 1), "a"), |_| None);
        assert_eq!(publication.summaries().len(), 1);
        assert_eq!(publication.job(), None);
    }

    #[test]
    fn short_pairs_join_upward_until_one_is_too_long() {
        let long = "x".repeat(SUMMARY_BYTES);
        let built = BTreeMap::from([
            (node(0, 1), "a"),
            (node(2, 2), "b\nc"),
            (node(4, 4), long.as_str()),
        ]);
        let publication = Publication::new(Summary::new(node(1, 1), "z"), lookup(&built));
        let published: Vec<_> = publication
            .summaries()
            .iter()
            .map(|summary| (summary.node(), summary.text().to_owned()))
            .collect();
        assert_eq!(
            published,
            [
                (node(1, 1), "z".to_owned()),
                (node(0, 2), "a\nz".to_owned()),
                (node(0, 4), "a\nz\nb\nc".to_owned()),
            ]
        );
        assert_eq!(publication.job(), Some(node(0, 8)));
        assert_eq!(publication.text(node(0, 2)).as_deref(), Some("a\nz"));
        assert_eq!(publication.text(node(0, 8)), None);
    }

    #[test]
    fn a_pair_exactly_at_the_limit_still_joins() {
        let half = "y".repeat(SUMMARY_BYTES / 2);
        let built = BTreeMap::from([(node(0, 1), &half[1..])]);
        let publication = Publication::new(Summary::new(node(1, 1), half.as_str()), lookup(&built));
        assert_eq!(publication.summaries()[1].text().len(), SUMMARY_BYTES);
        assert_eq!(publication.job(), None);
    }
}

use std::fmt::Write;
use std::sync::Arc;

use crate::{Error, Node};

/// An immutable completed summary with its permanent node identity.
///
/// Callers supply published records from the archive. This value does not prove
/// child readiness or enforce archive-wide uniqueness: those checks belong to
/// publication in the service. Loading a summary never loads its descendants.
/// Text can contain newlines and exceed the summarizer's 512-byte target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    node: Node,
    text: Arc<str>,
}

impl Summary {
    /// Own the supplied completed text, sharing it immutably across clones.
    #[must_use]
    pub fn new(node: Node, text: impl AsRef<str>) -> Self {
        Self {
            node,
            text: Arc::from(text.as_ref()),
        }
    }

    /// The range identity used for coverage, retrieval, and saving a view.
    #[must_use]
    pub const fn node(&self) -> Node {
        self.node
    }

    /// The completed text, unchanged from the supplied record.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn rendered_bytes(&self) -> Result<usize, Error> {
        format!("{}|", self.node)
            .len()
            .checked_add(self.text.len())
            .and_then(|size| size.checked_add(1))
            .ok_or(Error::Overflow)
    }

    pub(crate) fn render_into(&self, rendered: &mut String) {
        // Neither String's writer nor Node's formatter can return an error.
        let _ = write!(rendered, "{}|", self.node);
        for character in self.text.chars() {
            rendered.push(if matches!(character, '\r' | '\n') {
                ' '
            } else {
                character
            });
        }
        rendered.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_text_independently_of_the_supplied_buffer() {
        let node = Node::new(40, 8).unwrap();
        let mut source = String::from("user: keep this");
        let summary = Summary::new(node, &source);
        let copy = summary.clone();
        source.clear();
        assert_eq!(summary.node(), node);
        assert_eq!(summary.text(), "user: keep this");
        assert_eq!(copy, summary);
    }

    #[test]
    fn line_encoding_and_byte_count_agree_for_utf8_and_newlines() {
        let summary = Summary::new(Node::new(40, 8).unwrap(), "user: café\r\n🙂");
        let mut rendered = String::new();
        summary.render_into(&mut rendered);
        assert_eq!(rendered, "40+8|user: café  🙂\n");
        assert_eq!(summary.text(), "user: café\r\n🙂");
        assert_eq!(summary.rendered_bytes(), Ok(rendered.len()));
    }

    #[test]
    fn accepts_oversized_text_and_coarse_nodes_without_descendants() {
        let text = "🙂".repeat(200);
        let summary = Summary::new(Node::new(0, 1024).unwrap(), &text);
        assert_eq!(summary.text(), text);
        let mut rendered = String::new();
        summary.render_into(&mut rendered);
        assert_eq!(summary.rendered_bytes(), Ok(rendered.len()));
        assert!(summary.text().len() > 512);
    }
}

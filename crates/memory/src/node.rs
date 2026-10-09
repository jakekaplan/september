use std::fmt;

use crate::Error;

/// An aligned power-of-two message range, identified as `start+length`.
///
/// Message IDs are zero-based. The end is exclusive; neither an empty range nor
/// a range whose exclusive end overflows `u64` can be represented.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Node {
    start: u64,
    length: u64,
}

impl Node {
    /// Validate a node's permanent range identity.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidNode`] for an empty, non-power-of-two, unaligned,
    /// or overflowing range.
    pub fn new(start: u64, length: u64) -> Result<Self, Error> {
        if !length.is_power_of_two()
            || !start.is_multiple_of(length)
            || start.checked_add(length).is_none()
        {
            return Err(Error::InvalidNode { start, length });
        }
        Ok(Self { start, length })
    }

    /// The first message ID covered by this node.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// The number of messages covered by this node.
    #[must_use]
    pub const fn length(self) -> u64 {
        self.length
    }

    /// The exclusive end of this node's range.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.start + self.length
    }

    /// The left and right children, or `None` for an original-message leaf.
    #[must_use]
    pub const fn children(self) -> Option<[Self; 2]> {
        if self.length == 1 {
            return None;
        }
        let length = self.length / 2;
        Some([
            Self {
                start: self.start,
                length,
            },
            Self {
                start: self.start + length,
                length,
            },
        ])
    }

    /// The aligned parent containing this node and its sibling.
    ///
    /// This computes a range, not whether that parent's summary is built.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] if the parent's length or end exceeds `u64`.
    pub fn parent(self) -> Result<Self, Error> {
        let length = self.length.checked_mul(2).ok_or(Error::Overflow)?;
        let start = self.start - self.start % length;
        start.checked_add(length).ok_or(Error::Overflow)?;
        Ok(Self { start, length })
    }
}

impl fmt::Display for Node {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}+{}", self.start, self.length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_aligned_nonempty_power_of_two_ranges() {
        for (start, length) in [(0, 0), (0, 3), (1, 2), (u64::MAX, 1), (1 << 63, 1 << 63)] {
            assert_eq!(
                Node::new(start, length),
                Err(Error::InvalidNode { start, length })
            );
        }
        let node = Node::new(40, 8).unwrap();
        assert_eq!((node.start(), node.length(), node.end()), (40, 8, 48));
        assert_eq!(node.to_string(), "40+8");
    }

    #[test]
    fn children_and_parents_share_the_exact_range() {
        let node = Node::new(40, 8).unwrap();
        let children = node.children().unwrap();
        assert_eq!(
            children,
            [Node::new(40, 4).unwrap(), Node::new(44, 4).unwrap()]
        );
        assert_eq!(children[0].end(), children[1].start());
        assert_eq!(children[1].end(), node.end());
        assert_eq!(children[0].parent(), Ok(node));
        assert_eq!(children[1].parent(), Ok(node));
        assert_eq!(node.parent(), Node::new(32, 16));
        assert_eq!(Node::new(7, 1).unwrap().children(), None);
    }

    #[test]
    fn checks_the_largest_representable_ranges() {
        let largest = Node::new(0, 1 << 63).unwrap();
        assert_eq!(largest.parent(), Err(Error::Overflow));
        for child in largest.children().unwrap() {
            assert_eq!(child.parent(), Ok(largest));
        }
        let last = Node::new(u64::MAX - 1, 1).unwrap();
        assert_eq!(last.end(), u64::MAX);
        assert_eq!(last.parent(), Err(Error::Overflow));
    }
}

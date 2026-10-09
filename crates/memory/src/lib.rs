//! Deterministic memory tree geometry, view selection, and snapshot rules.
//!
//! Callers supply immutable completed [`Summary`] records. A [`View`] owns only
//! its current cover, with batched UTF-8 byte budgets and saved shrinking state.
//! Restoring a coarse summary does not require loading its descendants.
//!
//! A frozen [`Snapshot`] can zoom only into its cover nodes and their descendants.
//! [`Zoom`] selects child ranges or an original-message ID; content lookup belongs
//! to the service, not to a live tree collection inside this crate.
//!
//! There are no database, network, runtime, model, or harness dependencies. The
//! service must enforce archive-wide uniqueness, immutable publication, and child
//! readiness before supplying completed records. This crate does not publish
//! summaries, persist state, or hold original messages.
//!
//! ```
//! use september_memory::{Budget, Node, Summary, View, Zoom};
//!
//! let leaf = Node::new(0, 1)?;
//! let mut view = View::new(Budget::CHAT);
//! // No parent is built yet, so the view cannot merge.
//! view.append(Summary::new(leaf, "user: remember the decision"), |_| None)?;
//! let snapshot = view.freeze(1)?;
//! assert_eq!(snapshot.zoom(leaf)?, Zoom::Message(0));
//! # Ok::<(), september_memory::Error>(())
//! ```

mod error;
mod node;
mod snapshot;
mod summary;
mod view;

pub use error::Error;
pub use node::Node;
pub use snapshot::{Snapshot, Zoom};
pub use summary::Summary;
pub use view::{Budget, View};

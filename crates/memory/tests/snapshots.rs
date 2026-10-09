//! Acceptance tests across supplied summaries, live views, and frozen interactions.

#![cfg(test)]

use std::collections::BTreeMap;
use std::sync::Arc;

use september_memory::{Budget, Error, Node, Summary, View, Zoom};

fn node(start: u64, length: u64) -> Node {
    Node::new(start, length).unwrap()
}

fn nodes(view: &View) -> Vec<Node> {
    view.summaries().iter().map(Summary::node).collect()
}

fn none(_: Node) -> Option<Arc<str>> {
    None
}

fn lookup(parents: &BTreeMap<Node, String>) -> impl Fn(Node) -> Option<Arc<str>> + '_ {
    |node| parents.get(&node).map(|text| Arc::from(text.as_str()))
}

// These are supplied ready facts, not a worker or a full-history tree replica.
fn ready_parents(count: u64) -> BTreeMap<Node, String> {
    let mut parents = BTreeMap::new();
    let mut width = 2;
    while width <= count {
        for start in (0..=count - width).step_by(usize::try_from(width).unwrap()) {
            parents.insert(node(start, width), "p".into());
        }
        width *= 2;
    }
    parents
}

#[test]
fn frozen_snapshot_survives_new_messages_and_live_view_merges() {
    let mut view = View::new(Budget::new(20, 60).unwrap());
    let mut archive = BTreeMap::new();
    for id in 0..4 {
        archive.insert(
            id,
            format!("Original {id}: exact source, not its summary. 🙂"),
        );
        view.append(
            Summary::new(node(id, 1), "0123456789"),
            lookup(&ready_parents(id + 1)),
        )
        .unwrap();
    }
    let first = view.freeze(4).unwrap();
    let first_render = first.render().to_owned();
    assert_eq!(first.nodes(), &[node(0, 4)]);

    // Another session contributes while the first interaction remains active.
    for id in 4..8 {
        archive.insert(id, format!("Another session's original {id}"));
        view.append(
            Summary::new(node(id, 1), "0123456789"),
            lookup(&ready_parents(id + 1)),
        )
        .unwrap();
    }
    assert_eq!(nodes(&view), [node(0, 8)]);
    assert_eq!(first.cutoff(), 4);
    assert_eq!(first.nodes(), &[node(0, 4)]);
    assert_eq!(first.render(), first_render);
    let Zoom::Children(children) = first.zoom(node(0, 4)).unwrap() else {
        panic!("parent zoom must select child ranges");
    };
    assert_eq!(children, [node(0, 2), node(2, 2)]);
    let published = ready_parents(8);
    assert_eq!(&published[&children[0]], "p"); // Content lookup is outside the core.
    let Zoom::Message(id) = first.zoom(node(3, 1)).unwrap() else {
        panic!("leaf zoom must identify the original message");
    };
    assert_eq!(
        archive[&id],
        "Original 3: exact source, not its summary. 🙂"
    );
    assert!(!first.render().contains(&archive[&id]));
    for future in [node(4, 1), node(4, 4), node(0, 8)] {
        assert_eq!(first.zoom(future), Err(Error::OutsideSnapshot(future)));
    }
    let next = view.freeze(8).unwrap();
    assert_eq!(next.zoom(node(4, 1)), Ok(Zoom::Message(4)));
    assert_eq!(first.render(), first_render);
}

#[test]
fn restoring_the_saved_cover_preserves_the_append_only_rendered_prefix() {
    let mut published = BTreeMap::new();
    let mut view = View::new(Budget::CHAT);
    for id in 0..4 {
        let text = format!("user: message {id}");
        published.insert(node(id, 1), text.clone());
        view.append(
            Summary::new(node(id, 1), text),
            lookup(&ready_parents(id + 1)),
        )
        .unwrap();
    }
    let snapshot = view.freeze(4).unwrap();
    let saved_nodes = nodes(&view);
    let saved_shrinking = view.is_shrinking();

    // Load just the referenced records. Available ancestors must not refit it.
    published.extend(ready_parents(4));
    let loaded_cover = saved_nodes
        .iter()
        .map(|id| Summary::new(*id, published[id].as_str()))
        .collect();
    let mut loaded = View::restore(loaded_cover, saved_shrinking, Budget::CHAT).unwrap();
    assert_eq!(nodes(&loaded), saved_nodes);
    assert_eq!(loaded.summaries().len(), 4);
    assert_eq!(loaded.render().unwrap(), snapshot.render());
    published.clear(); // Rendering does not depend on keeping a backing map alive.
    loaded
        .append(Summary::new(node(4, 1), "user: new interaction"), none)
        .unwrap();
    let prefix = snapshot.render().strip_suffix("</chat>").unwrap();
    assert!(loaded.render().unwrap().starts_with(prefix));
    assert_eq!(snapshot.cutoff(), 4);
    assert_eq!(
        snapshot.zoom(node(4, 1)),
        Err(Error::OutsideSnapshot(node(4, 1)))
    );
}

#[test]
fn an_unbuilt_message_cannot_be_replaced_by_a_placeholder_or_stale_snapshot() {
    let mut view = View::new(Budget::CHAT);
    view.append(Summary::new(node(0, 1), "first"), none)
        .unwrap();
    let early = Summary::new(node(2, 1), "third finished early");
    assert_eq!(view.append(early.clone(), none), Err(Error::InvalidView));
    assert_eq!(
        view.freeze(3),
        Err(Error::CutoffMismatch {
            expected: 3,
            actual: 1
        })
    );
    assert_eq!(
        view.freeze(0),
        Err(Error::CutoffMismatch {
            expected: 0,
            actual: 1
        })
    );
    assert_eq!(nodes(&view), [node(0, 1)]);
    view.append(Summary::new(node(1, 1), "second is now complete"), none)
        .unwrap();
    view.append(early, none).unwrap();
    let ready = view.freeze(3).unwrap();
    assert_eq!(ready.cutoff(), 3);
    assert_eq!(ready.zoom(node(1, 1)), Ok(Zoom::Message(1)));
}

#[test]
fn many_appends_batches_and_restores_keep_exact_coverage_and_frozen_reads() {
    let budget = Budget::new(70, 130).unwrap();
    let mut view = View::new(budget);
    let mut snapshots = Vec::new();
    for id in 0..128 {
        let leaf = node(id, 1);
        let text = format!("user: {id} café 🙂");
        let old_render = view.render().unwrap();
        let append_only = !view.is_shrinking()
            && old_render.len() + format!("{leaf}|{text}\n").len() <= budget.trigger();
        view.append(Summary::new(leaf, text), lookup(&ready_parents(id + 1)))
            .unwrap();
        let rendered = view.render().unwrap();
        assert_eq!(view.rendered_bytes().unwrap(), rendered.len());
        if append_only {
            assert!(rendered.starts_with(old_render.strip_suffix("</chat>").unwrap()));
        }
        let mut next = 0;
        for summary in view.summaries() {
            let covered = summary.node();
            assert_eq!(covered.start(), next);
            assert!(covered.length().is_power_of_two());
            assert!(covered.start().is_multiple_of(covered.length()));
            next = covered.end();
        }
        assert_eq!(next, id + 1);
        if view.is_shrinking() {
            assert!(rendered.len() > budget.target());
        } else {
            assert!(rendered.len() <= budget.trigger());
        }
        let restored =
            View::restore(view.summaries().to_vec(), view.is_shrinking(), budget).unwrap();
        assert_eq!(restored, view);
        snapshots.push((restored.freeze(id + 1).unwrap(), rendered));
        view = restored;
    }
    for (snapshot, rendered) in snapshots {
        assert_eq!(snapshot.render(), rendered);
        let last = snapshot.cutoff() - 1;
        assert_eq!(snapshot.zoom(node(last, 1)), Ok(Zoom::Message(last)));
        let future = node(snapshot.cutoff(), 1);
        assert_eq!(snapshot.zoom(future), Err(Error::OutsideSnapshot(future)));
    }
}

#[test]
fn failed_view_updates_preserve_the_live_view_and_ready_interaction() {
    let mut view = View::new(Budget::CHAT);
    let parents = BTreeMap::new();
    assert_eq!(
        view.append(Summary::new(node(0, 2), "parent"), lookup(&parents)),
        Err(Error::InvalidLeaf(node(0, 2)))
    );
    assert_eq!(
        view.append(Summary::new(node(1, 1), "skipped"), lookup(&parents)),
        Err(Error::InvalidView)
    );
    assert_eq!(view.summaries(), []);
    view.append(Summary::new(node(0, 1), "leaf"), lookup(&parents))
        .unwrap();
    let saved_view = view.clone();
    let snapshot = view.freeze(1).unwrap();
    assert_eq!(
        view.append(Summary::new(node(0, 1), "duplicate"), lookup(&parents)),
        Err(Error::InvalidView)
    );
    assert_eq!(
        view.append(Summary::new(node(2, 1), "gap"), lookup(&parents)),
        Err(Error::InvalidView)
    );
    assert_eq!(view, saved_view);
    assert_eq!(view.freeze(1).unwrap(), snapshot);
    assert_eq!(snapshot.zoom(node(0, 1)), Ok(Zoom::Message(0)));
}

#[test]
fn failed_updates_preserve_an_unfinished_batch_and_its_snapshot() {
    let mut view = View::new(Budget::new(20, 60).unwrap());
    for id in 0..4 {
        view.append(Summary::new(node(id, 1), "0123456789"), none)
            .unwrap();
    }
    assert!(view.is_shrinking());
    let saved = view.clone();
    let snapshot = view.freeze(4).unwrap();
    assert_eq!(
        view.append(Summary::new(node(0, 2), "parent"), none),
        Err(Error::InvalidLeaf(node(0, 2)))
    );
    assert_eq!(
        view.append(Summary::new(node(5, 1), "gap"), none),
        Err(Error::InvalidView)
    );
    assert_eq!(view, saved);
    view.compact(lookup(&ready_parents(4))).unwrap();
    assert!(!view.is_shrinking());
    assert_eq!(nodes(&view), [node(0, 4)]);
    assert_eq!(
        snapshot.nodes(),
        &[node(0, 1), node(1, 1), node(2, 1), node(3, 1)]
    );
}

#[test]
fn one_coarse_saved_summary_restores_without_loading_any_descendants() {
    let root = node(0, 1024);
    let view = View::restore(
        vec![Summary::new(root, "published summary of 1024 messages")],
        false,
        Budget::CHAT,
    )
    .unwrap();
    assert_eq!(view.summaries().len(), 1);
    let snapshot = view.freeze(1024).unwrap();
    assert_eq!(snapshot.nodes(), &[root]);
    assert_eq!(
        snapshot.zoom(root),
        Ok(Zoom::Children([node(0, 512), node(512, 512)]))
    );
    assert_eq!(
        snapshot.zoom(node(512, 512)),
        Ok(Zoom::Children([node(512, 256), node(768, 256)]))
    );
    assert_eq!(snapshot.zoom(node(1023, 1)), Ok(Zoom::Message(1023)));
}

#[test]
fn later_parent_completion_cannot_expand_frozen_navigation() {
    let mut parents = BTreeMap::new();
    let mut view = View::new(Budget::new(20, 30).unwrap());
    for id in 0..2 {
        view.append(Summary::new(node(id, 1), "0123456789"), lookup(&parents))
            .unwrap();
    }
    let snapshot = view.freeze(2).unwrap();
    let rendered = snapshot.render().to_owned();
    let parent = node(0, 2);
    assert_eq!(snapshot.zoom(parent), Err(Error::OutsideSnapshot(parent)));
    assert_eq!(snapshot.zoom(node(1, 1)), Ok(Zoom::Message(1)));
    parents.insert(parent, "p".into());
    view.compact(lookup(&parents)).unwrap();
    assert_eq!(nodes(&view), [parent]);
    assert_eq!(snapshot.zoom(parent), Err(Error::OutsideSnapshot(parent)));
    assert_eq!(snapshot.render(), rendered);
    assert_eq!(snapshot.cutoff(), 2);
    let next = view.freeze(2).unwrap();
    assert_eq!(
        next.zoom(parent),
        Ok(Zoom::Children([node(0, 1), node(1, 1)]))
    );
}

use std::collections::BTreeMap;

use super::*;

fn node(start: u64, length: u64) -> Node {
    Node::new(start, length).unwrap()
}

fn lookup(built: &BTreeMap<Node, String>) -> impl Fn(Node) -> Option<Arc<str>> + '_ {
    |node| built.get(&node).map(|text| Arc::from(text.as_str()))
}

/// Leaves `0..count` rendering as 15-byte lines, plus every aligned parent if asked.
fn archive(count: u64, parents: bool) -> BTreeMap<Node, String> {
    let mut built: BTreeMap<_, _> = (0..count)
        .map(|id| (node(id, 1), "0123456789".to_owned()))
        .collect();
    let mut width = 2;
    while parents && width <= count {
        for start in (0..=count - width).step_by(usize::try_from(width).unwrap()) {
            built.insert(node(start, width), "p".into());
        }
        width *= 2;
    }
    built
}

fn nodes(view: &View) -> Vec<Node> {
    view.summaries().iter().map(Summary::node).collect()
}

#[test]
fn advance_appends_built_leaves_and_returns_only_wanted_cutoffs() {
    let mut views = Views::new(Budget::CHAT, Budget::COMPACTION);
    let built = archive(3, false);
    let reached = views
        .advance(lookup(&built), |cutoff| cutoff == 0 || cutoff == 2)
        .unwrap();
    assert_eq!(reached.iter().map(View::cutoff).collect::<Vec<_>>(), [0, 2]);
    assert_eq!(views.live().cutoff(), 3);
}

#[test]
fn the_compaction_view_merges_on_its_own_without_changing_the_live_view() {
    let mut views = Views::new(Budget::CHAT, Budget::new(20, 40).unwrap());
    let built = archive(4, true);
    views.advance(lookup(&built), |_| false).unwrap();
    assert_eq!(views.live().summaries().len(), 4);
    assert_eq!(nodes(&views.compaction), [node(0, 4)]);
}

#[test]
fn a_live_merge_derives_the_compaction_view_again() {
    let mut views = Views::new(Budget::new(30, 60).unwrap(), Budget::new(20, 40).unwrap());
    let built = archive(4, false);
    views.advance(lookup(&built), |_| false).unwrap();
    // Without parents both views wait in an unfinished batch.
    assert_eq!(nodes(&views.compaction), nodes(views.live()));
    let built = archive(4, true);
    views.advance(lookup(&built), |_| false).unwrap();
    assert_eq!(nodes(views.live()), [node(0, 2), node(2, 2)]);
    assert_eq!(nodes(&views.compaction), [node(0, 4)]);
    assert!(!views.compaction.is_shrinking());
}

#[test]
fn context_ends_before_a_leaf_or_after_a_parent_and_at_the_first_gap() {
    let mut views = Views::new(Budget::CHAT, Budget::COMPACTION);
    views
        .advance(lookup(&archive(3, false)), |_| false)
        .unwrap();
    let cutoff = |node| views.context(node).unwrap().unwrap().cutoff();
    assert_eq!(cutoff(node(2, 1)), 2);
    assert_eq!(cutoff(node(0, 2)), 2);
    // Leaf 3 is unbuilt, so nothing later can be shown.
    assert_eq!(cutoff(node(4, 1)), 3);
}

#[test]
fn context_waits_while_it_exceeds_the_compaction_trigger() {
    let mut views = Views::new(Budget::CHAT, Budget::new(20, 40).unwrap());
    views
        .advance(lookup(&archive(4, false)), |_| false)
        .unwrap();
    assert_eq!(views.context(node(1, 1)).unwrap().unwrap().cutoff(), 1);
    assert_eq!(views.context(node(2, 1)), Ok(None));
}

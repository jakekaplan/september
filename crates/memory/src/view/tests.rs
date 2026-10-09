use std::collections::BTreeMap;

use super::*;

fn node(start: u64, length: u64) -> Node {
    Node::new(start, length).unwrap()
}

fn summary(start: u64, length: u64, text: &str) -> Summary {
    Summary::new(node(start, length), text)
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
fn validates_budgets_and_counts_actual_utf8_rendering() {
    for (target, trigger) in [(0, 1), (13, 100), (100, 100), (101, 100)] {
        assert_eq!(
            Budget::new(target, trigger),
            Err(Error::InvalidBudget { target, trigger })
        );
    }
    assert_eq!(
        (Budget::CHAT.target(), Budget::CHAT.trigger()),
        (64_000, 128_000)
    );
    assert_eq!(
        (Budget::COMPACTION.target(), Budget::COMPACTION.trigger()),
        (16_000, 32_000)
    );
    let mut view = View::new(Budget::CHAT);
    view.append(summary(0, 1, "user: café\n🙂\rnext"), none)
        .unwrap();
    assert_eq!(
        view.render().unwrap(),
        "<chat>\n0+1|user: café 🙂 next\n</chat>"
    );
    assert_eq!(view.rendered_bytes().unwrap(), view.render().unwrap().len());
}

#[test]
fn only_crossing_the_trigger_starts_a_batch() {
    let parents = ready_parents(4);
    let mut view = View::new(Budget::new(31, 36).unwrap());
    for id in 0..2 {
        view.append(summary(id, 1, "abcdef"), lookup(&parents))
            .unwrap();
    }
    assert_eq!(view.rendered_bytes(), Ok(36));
    assert_eq!(nodes(&view), [node(0, 1), node(1, 1)]);
    assert!(!view.is_shrinking());
    assert_eq!(view.compact(lookup(&parents)), Ok(0));
    assert_eq!(nodes(&view), [node(0, 1), node(1, 1)]);
    assert_eq!(
        view.append(summary(2, 1, "abcdef"), lookup(&parents)),
        Ok(1)
    );
    assert_eq!(nodes(&view), [node(0, 2), node(2, 1)]);
    assert_eq!(view.rendered_bytes(), Ok(31));
    assert!(!view.is_shrinking());
}

#[test]
fn one_batch_merges_repeatedly_until_the_target() {
    let parents = ready_parents(4);
    let mut view = View::new(Budget::new(20, 60).unwrap());
    for id in 0..3 {
        view.append(summary(id, 1, "0123456789"), lookup(&parents))
            .unwrap();
    }
    assert_eq!(view.summaries().len(), 3);
    assert_eq!(
        view.append(summary(3, 1, "0123456789"), lookup(&parents)),
        Ok(3)
    );
    assert_eq!(nodes(&view), [node(0, 4)]);
    assert_eq!(view.rendered_bytes(), Ok(20));
    assert!(!view.is_shrinking());
}

#[test]
fn a_blocked_batch_survives_restore_and_resumes_below_the_trigger() {
    let mut parents = BTreeMap::new();
    let budget = Budget::new(20, 60).unwrap();
    let mut view = View::new(budget);
    for id in 0..4 {
        view.append(summary(id, 1, "0123456789"), lookup(&parents))
            .unwrap();
    }
    assert!(view.is_shrinking());
    let blocked = view.clone();
    view.compact(lookup(&parents)).unwrap();
    assert_eq!(view, blocked);
    parents.insert(node(0, 2), "p".into());
    view.compact(lookup(&parents)).unwrap();
    assert_eq!(view.rendered_bytes(), Ok(50));
    let mut restored =
        View::restore(view.summaries().to_vec(), view.is_shrinking(), budget).unwrap();
    assert_eq!(restored.render(), view.render());
    parents.insert(node(2, 2), "p".into());
    restored.compact(lookup(&parents)).unwrap();
    assert_eq!(nodes(&restored), [node(0, 2), node(2, 2)]);
    assert!(restored.is_shrinking());
    parents.insert(node(0, 4), "p".into());
    restored.compact(lookup(&parents)).unwrap();
    assert_eq!(nodes(&restored), [node(0, 4)]);
    assert!(!restored.is_shrinking());
}

#[test]
fn pending_batches_also_resume_on_append() {
    let budget = Budget::new(20, 60).unwrap();
    let mut view = View::new(budget);
    for id in 0..4 {
        view.append(summary(id, 1, "0123456789"), none).unwrap();
    }
    assert!(view.is_shrinking());
    view.append(summary(4, 1, "next"), lookup(&ready_parents(5)))
        .unwrap();
    assert_eq!(nodes(&view), [node(0, 4), node(4, 1)]);
    assert!(view.is_shrinking());
    assert!(view.rendered_bytes().unwrap() < budget.trigger());
}

#[test]
fn ranking_uses_the_pairs_last_message_not_first_or_exclusive_end() {
    let text = "x".repeat(40);
    let cover = vec![
        summary(0, 4, &text),
        summary(4, 4, &text),
        summary(8, 1, &text),
        summary(9, 1, &text),
    ];
    let mut view = View::restore(cover, true, Budget::new(160, 180).unwrap()).unwrap();
    let parents = BTreeMap::from([(node(0, 8), "p".into()), (node(8, 2), "p".into())]);
    view.compact(lookup(&parents)).unwrap();
    assert_eq!(nodes(&view), [node(0, 4), node(4, 4), node(8, 2)]);
}

#[test]
fn equal_due_pairs_choose_the_oldest_even_across_levels() {
    let text = "x".repeat(40);
    let cover = vec![
        summary(0, 2, "p"),
        summary(2, 2, "p"),
        summary(4, 1, &text),
        summary(5, 1, &text),
        summary(6, 1, &text),
    ];
    let mut view = View::restore(cover, true, Budget::new(155, 160).unwrap()).unwrap();
    view.compact(lookup(&ready_parents(7))).unwrap();
    assert_eq!(
        nodes(&view),
        [node(0, 4), node(4, 1), node(5, 1), node(6, 1)]
    );
    assert!(!view.is_shrinking());
}

#[test]
fn ranks_large_sparse_ranges_exactly_when_float_scores_would_tie() {
    let scale = 1_u64 << 59;
    // Just above the cross-level tie: the more recent pair is slightly more due.
    let cutoff = u64::try_from((32 * u128::from(scale) - 3).div_ceil(3)).unwrap();
    let text = "x".repeat(40);
    let mut cover = vec![
        summary(0, 4 * scale, &text),
        summary(4 * scale, 4 * scale, &text),
        summary(8 * scale, scale, &text),
        summary(9 * scale, scale, &text),
    ];
    let mut cursor = 10 * scale;
    while cursor < cutoff {
        let mut width = 1 << ((cutoff - cursor).ilog2());
        while !cursor.is_multiple_of(width) {
            width /= 2;
        }
        cover.push(summary(cursor, width, "p"));
        cursor += width;
    }
    let bytes = View::restore(cover.clone(), false, Budget::CHAT)
        .unwrap()
        .rendered_bytes()
        .unwrap();
    let recent = node(8 * scale, 2 * scale);
    let removed = cover[2].rendered_bytes().unwrap() + cover[3].rendered_bytes().unwrap();
    let added = Summary::new(recent, "p").rendered_bytes().unwrap();
    let mut view = View::restore(
        cover,
        true,
        Budget::new(bytes - removed + added, bytes - 1).unwrap(),
    )
    .unwrap();
    let parents = BTreeMap::from([(node(0, 8 * scale), "p".into()), (recent, "p".into())]);
    view.compact(lookup(&parents)).unwrap();
    assert_eq!(
        &nodes(&view)[..3],
        &[node(0, 4 * scale), node(4 * scale, 4 * scale), recent]
    );
    assert_eq!(view.cutoff(), cutoff);
    assert!(!view.is_shrinking());
}

#[test]
fn chooses_a_ready_pair_when_the_more_due_parent_is_not_ready() {
    let text = "x".repeat(40);
    let cover = (0..4).map(|id| summary(id, 1, &text)).collect();
    let mut view = View::restore(cover, true, Budget::new(160, 180).unwrap()).unwrap();
    view.compact(lookup(&BTreeMap::from([(node(2, 2), "p".into())])))
        .unwrap();
    assert_eq!(nodes(&view), [node(0, 1), node(1, 1), node(2, 2)]);
}

#[test]
fn merging_supplied_coarse_summaries_needs_only_the_ready_parent() {
    let cover = vec![summary(0, 2, "p"), summary(2, 2, "p")];
    let mut view = View::restore(cover, true, Budget::new(20, 25).unwrap()).unwrap();
    let mut parents = BTreeMap::from([(node(0, 4), "p".into())]);
    view.compact(lookup(&parents)).unwrap();
    parents.clear();
    assert_eq!(nodes(&view), [node(0, 4)]);
    assert_eq!(view.render().unwrap(), "<chat>\n0+4|p\n</chat>");
    assert!(!view.is_shrinking());
}

#[test]
fn restore_validates_cover_and_batch_state_without_refitting() {
    for cover in [
        vec![summary(1, 1, "p")],
        vec![summary(0, 1, "p"), summary(2, 1, "p")],
        vec![summary(0, 2, "p"), summary(1, 1, "p")],
    ] {
        assert_eq!(
            View::restore(cover, false, Budget::CHAT),
            Err(Error::InvalidView)
        );
    }
    let cover: Vec<_> = (0..4).map(|id| summary(id, 1, "0123456789")).collect();
    let restored = View::restore(cover.clone(), false, Budget::CHAT).unwrap();
    assert_eq!(restored.summaries(), cover);
    assert_eq!(
        View::restore(cover, false, Budget::new(20, 60).unwrap()),
        Err(Error::InvalidView)
    );
    assert_eq!(
        View::restore(vec![summary(0, 4, "p")], true, Budget::CHAT),
        Err(Error::InvalidView)
    );
    assert_eq!(
        View::restore(vec![], true, Budget::CHAT),
        Err(Error::InvalidView)
    );
}

#[test]
fn oversized_summaries_are_counted_and_do_not_make_a_blocked_batch_spin() {
    let mut view = View::new(Budget::new(20, 29).unwrap());
    for id in 0..2 {
        view.append(summary(id, 1, "short"), none).unwrap();
    }
    view.compact(lookup(&BTreeMap::from([(node(0, 2), "🙂".repeat(200))])))
        .unwrap();
    assert_eq!(nodes(&view), [node(0, 2)]);
    assert_eq!(view.rendered_bytes(), Ok(819));
    assert!(view.is_shrinking());
    let saved = view.clone();
    view.compact(none).unwrap();
    assert_eq!(view, saved);
}

#[test]
fn historical_prefix_requires_a_whole_cover_boundary() {
    let view = View::restore(
        vec![summary(0, 2, "past"), summary(2, 2, "later")],
        false,
        Budget::CHAT,
    )
    .unwrap();
    assert_eq!(view.prefix(0).unwrap().render().unwrap(), "<chat>\n</chat>");
    assert_eq!(
        view.prefix(2).unwrap().render().unwrap(),
        "<chat>\n0+2|past\n</chat>"
    );
    for cutoff in [1, 3, 5] {
        assert!(matches!(
            view.prefix(cutoff),
            Err(Error::CutoffMismatch { .. })
        ));
    }
    assert_eq!(view.prefix(4).unwrap(), view);
}

#[test]
fn resizing_batches_to_target_even_below_trigger_and_retains_blocked_state() {
    let mut view = View::new(Budget::CHAT);
    for id in 0..4 {
        view.append(summary(id, 1, "0123456789"), none).unwrap();
    }
    let budget = Budget::new(20, 100).unwrap();
    view.resize(budget, none).unwrap();
    assert!(view.is_shrinking());
    assert!(!view.prefix(0).unwrap().is_shrinking());
    assert!(view.prefix(2).unwrap().is_shrinking());
    let mut restored = View::restore(view.summaries().to_vec(), true, budget).unwrap();
    restored.compact(lookup(&ready_parents(4))).unwrap();
    assert_eq!(nodes(&restored), [node(0, 4)]);
    assert!(!restored.is_shrinking());
}

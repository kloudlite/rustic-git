use gix_hash::ObjectId;

/// What a bounded merge-base walk concluded. Three answers, not two: "no ancestor within the
/// budget" used to collapse into `None`, and callers read that as "unrelated" — which on a
/// long-lived branch of a big repo recorded a perfectly mergeable change as Dirty and hid its
/// merge button. Running out of budget is "I do not know", and must stay distinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeBase {
    Found(ObjectId),
    /// Both histories were walked to their roots and share nothing.
    Unrelated,
    /// The walk stopped at the budget before either history was exhausted.
    Exhausted,
}

/// The best common ancestor of two commits — where a branch left the one it
/// wants back into.
///
/// Bounded, like every other walk here: `Exhausted` when the answer would take more than
/// `budget` commits, which callers must treat as unknown rather than guessing either way.
pub fn merge_base(odb: &gix_odb::Handle, a: ObjectId, b: ObjectId, budget: usize) -> MergeBase {
    if a == b {
        return MergeBase::Found(a);
    }
    // Everything reachable from `a`, then the first of `b`'s ancestors in it.
    // First by generation rather than best-by-date: `Simple` walks newest-first,
    // so the first hit is the closest common ancestor for the histories a review
    // actually sees.
    let mut walked_a = 0;
    let seen: std::collections::HashSet<ObjectId> = gix_traverse::commit::Simple::new(Some(a), odb.clone())
        .take(budget)
        .inspect(|_| walked_a += 1)
        .filter_map(|i| i.ok().map(|i| i.id))
        .collect();
    if seen.contains(&b) {
        return MergeBase::Found(b);
    }
    let mut walked_b = 0;
    let hit = gix_traverse::commit::Simple::new(Some(b), odb.clone())
        .take(budget)
        .inspect(|_| walked_b += 1)
        .filter_map(|i| i.ok().map(|i| i.id))
        .find(|id| seen.contains(id));
    match hit {
        Some(m) => MergeBase::Found(m),
        // A walk that yielded exactly `budget` commits may have more behind it; only one that
        // ran dry on both sides proves the histories are disjoint.
        None if walked_a < budget && walked_b < budget => MergeBase::Unrelated,
        None => MergeBase::Exhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::{merge_base, MergeBase};
    use gix_hash::ObjectId;
    use gix_object::Write as _;

    fn commit(odb: &gix_odb::Handle, parents: &[ObjectId], msg: &str) -> ObjectId {
        let tree = odb.write(&gix_object::Tree::empty()).unwrap();
        let sig = gix_actor::Signature {
            name: "t".into(),
            email: "t@x".into(),
            time: gix_actor::date::Time::new(1_700_000_000, 0),
        };
        odb.write(&gix_object::Commit {
            tree,
            parents: parents.iter().copied().collect(),
            author: sig.clone(),
            committer: sig,
            encoding: None,
            message: msg.into(),
            extra_headers: vec![],
        })
        .unwrap()
    }

    #[test]
    fn exhausted_budget_is_not_unrelated() {
        let dir = std::env::temp_dir().join(format!("rg-mb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let odb = gix_odb::at(&dir).unwrap();
        let root = commit(&odb, &[], "root");
        let p = commit(&odb, &[root], "p");
        let q = commit(&odb, &[p], "q");
        let stray = commit(&odb, &[], "stray");

        assert_eq!(merge_base(&odb, q, p, 50), MergeBase::Found(p));
        assert_eq!(merge_base(&odb, q, stray, 50), MergeBase::Unrelated);
        // A budget of one sees only the tips: the answer is unknown, not "no shared history".
        assert_eq!(merge_base(&odb, q, p, 1), MergeBase::Exhausted);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

use gix_hash::ObjectId;

/// The best common ancestor of two commits — where a branch left the one it
/// wants back into.
///
/// Bounded, like every other walk here: `None` when the two have no ancestor
/// within `budget`, which callers read as "these are unrelated" and refuse to act
/// on rather than guessing.
pub fn merge_base(
    odb: &gix_odb::Handle,
    a: ObjectId,
    b: ObjectId,
    budget: usize,
) -> Option<ObjectId> {
    if a == b {
        return Some(a);
    }
    // Everything reachable from `a`, then the first of `b`'s ancestors in it.
    // First by generation rather than best-by-date: `Simple` walks newest-first,
    // so the first hit is the closest common ancestor for the histories a review
    // actually sees.
    let seen: std::collections::HashSet<ObjectId> = gix_traverse::commit::Simple::new(Some(a), odb.clone())
        .take(budget)
        .filter_map(|i| i.ok().map(|i| i.id))
        .collect();
    if seen.contains(&b) {
        return Some(b);
    }
    gix_traverse::commit::Simple::new(Some(b), odb.clone())
        .take(budget)
        .filter_map(|i| i.ok().map(|i| i.id))
        .find(|id| seen.contains(id))
}

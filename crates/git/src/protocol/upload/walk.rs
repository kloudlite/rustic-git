use gix_hash::ObjectId;
use gix_pack::data::output::count::objects::ObjectExpansion;
use crate::Result;
use std::sync::atomic::AtomicBool;

/// What a client asked us to leave OUT of the pack — partial clone.
///
/// History stays whole; the bulk does not. The client records the server as a
/// "promisor" and comes back for individual objects when it actually needs them,
/// which is why `Fetch::wants` has to allow more than ref tips once this is on.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Filter {
    /// No blobs at all — `blob:none`.
    NoBlobs,
    /// Blobs under this many bytes — `blob:limit=<n>`.
    BlobLimit(u64),
    /// Commits only, no trees and no blobs — `tree:0`.
    NoTrees,
}

impl Filter {
    pub(super) fn parse(spec: &str) -> Option<Filter> {
        match spec.trim() {
            "blob:none" => Some(Filter::NoBlobs),
            "tree:0" => Some(Filter::NoTrees),
            other => other
                .strip_prefix("blob:limit=")
                .and_then(parse_size)
                .map(Filter::BlobLimit),
        }
    }
}

/// `1024`, `10k`, `1m`, `1g` — the suffixes git itself accepts.
pub(super) fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, mult) = match s.chars().last()?.to_ascii_lowercase() {
        'k' => (&s[..s.len() - 1], 1024),
        'm' => (&s[..s.len() - 1], 1024 * 1024),
        'g' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    // checked: a client-supplied `blob:limit=<n><suffix>` filter multiplying overflow would wrap
    // to a small number, silently turning a huge limit into a near-zero one.
    digits.trim().parse::<u64>().ok().and_then(|n| n.checked_mul(mult))
}

/// Expand `commits` into the objects a filtered pack should carry.
///
/// Done here rather than by the packer's own tree expansion, because the whole
/// point is to decide per object whether it goes in — which is a decision the
/// "expand everything under these commits" mode cannot express.
pub(super) fn filtered_objects(
    odb: &gix_odb::Handle,
    commits: &[ObjectId],
    filter: Filter,
) -> Result<Vec<ObjectId>> {
    use gix_object::FindExt;
    use std::collections::HashSet;

    let mut out: Vec<ObjectId> = Vec::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut buf = Vec::new();

    // Commit objects always travel: a partial clone still has all of history.
    let mut trees: Vec<ObjectId> = Vec::new();
    for c in commits {
        if !seen.insert(*c) {
            continue;
        }
        out.push(*c);
        if filter == Filter::NoTrees {
            continue;
        }
        // A miss here would silently ship a pack with a hole in it — an object the client is
        // told it has and does not.
        if let gix_object::ObjectRef::Commit(commit) = FindExt::find(odb, c, &mut buf)?.decode()? {
            trees.push(commit.tree());
        }
    }

    while let Some(id) = trees.pop() {
        if !seen.insert(id) {
            continue;
        }
        let tree = odb.find_tree(&id, &mut buf)?;
        out.push(id);
        // Collected before the next find_tree call reuses the buffer.
        let entries: Vec<(ObjectId, bool)> = tree
            .entries
            .iter()
            .map(|e| (e.oid.to_owned(), e.mode.is_tree()))
            .collect();
        for (child, is_tree) in entries {
            if is_tree {
                trees.push(child);
            } else if seen.insert(child) && keep_blob(odb, child, filter) {
                out.push(child);
            }
        }
    }
    Ok(out)
}

/// A blob's SIZE decides `blob:limit`, and the size is in the object header —
/// so this never inflates a blob to find out whether to send it.
pub(super) fn keep_blob(odb: &gix_odb::Handle, id: ObjectId, filter: Filter) -> bool {
    match filter {
        Filter::NoBlobs | Filter::NoTrees => false,
        Filter::BlobLimit(max) => {
            use gix_object::FindHeader;
            odb.try_header(&id).ok().flatten().is_some_and(|h| h.size <= max)
        }
    }
}

/// What a client asked us to cut its history down to.
///
/// All three of git's ways of saying "less history" are the same walk with a
/// different stop condition, so they are one struct rather than three code paths.
#[derive(Default)]
pub(super) struct Deepen {
    /// `deepen <n>`: n commits back from each want. 1 means "just the tips".
    pub(super) depth: Option<usize>,
    /// `deepen-since <unix>`: nothing committed before this.
    pub(super) since: Option<i64>,
    /// `deepen-not <ref>`: stop when the walk reaches these.
    pub(super) not: Vec<ObjectId>,
    /// `shallow <oid>`: boundaries the client already has. It re-sends these every
    /// time, which is why the server keeps no per-client state.
    pub(super) client_shallow: Vec<ObjectId>,
    /// `deepen-relative`: depth counts from the client's existing boundary rather
    /// than from the tips.
    pub(super) relative: bool,
}

impl Deepen {
    pub(super) fn asked(&self) -> bool {
        self.depth.is_some() || self.since.is_some() || !self.not.is_empty()
    }
}

/// The commits a shallow fetch should send, and where its history is cut.
pub(super) struct Shallow {
    /// Every commit inside the boundary — what the pack will carry.
    pub(super) commits: Vec<ObjectId>,
    /// Commits whose parents are being withheld. The client records these as its
    /// new `.git/shallow`.
    pub(super) boundary: Vec<ObjectId>,
    /// Commits the client had as a boundary that are now complete. This is what
    /// `--unshallow` reports.
    pub(super) unshallow: Vec<ObjectId>,
}

/// Walk back from `wants`, stopping where the client asked.
///
/// Breadth-first by design: `depth` is measured in commits from the tip, so every
/// commit at distance n must be seen before any at n+1. A depth-first walk would
/// cut one long branch and leave a short one whole.
pub(super) fn shallow_walk(odb: &gix_odb::Handle, wants: &[ObjectId], d: &Deepen) -> Result<Shallow> {
    use std::collections::{HashMap, HashSet, VecDeque};

    let cut: HashSet<ObjectId> = d.not.iter().copied().collect();
    // With `deepen-relative` the client is asking for n MORE commits, so its
    // current boundary starts at distance 0 rather than being excluded.
    let mut depth_of: HashMap<ObjectId, usize> = HashMap::new();
    let mut queue: VecDeque<(ObjectId, usize)> = VecDeque::new();
    if d.relative {
        for c in &d.client_shallow {
            queue.push_back((*c, 0));
        }
    }
    for w in wants {
        queue.push_back((*w, 1));
    }

    let mut boundary = Vec::new();
    let mut buf = Vec::new();
    let mut pbuf = Vec::new();
    while let Some((id, depth)) = queue.pop_front() {
        if let Some(prev) = depth_of.get(&id) {
            if *prev <= depth {
                continue;
            }
        }
        let Ok(obj) = gix_object::FindExt::find(odb, &id, &mut buf) else { continue };
        let Ok(gix_object::ObjectRef::Commit(commit)) = obj.decode() else { continue };

        depth_of.insert(id, depth);

        // Would this commit's parents be inside the boundary?
        let deep_enough = d.depth.is_some_and(|max| depth >= max);
        let parents: Vec<ObjectId> = commit.parents().collect();

        if parents.is_empty() {
            // A root commit has no history to withhold, so it is not a boundary
            // even at the depth limit — saying otherwise makes a complete clone
            // claim to be shallow.
            continue;
        }
        if deep_enough || cut.contains(&id) {
            boundary.push(id);
            continue;
        }
        for p in parents {
            // `since` is checked on the PARENT here, before it would be queued —
            // never after insertion into depth_of — so a too-old commit never
            // enters the pack or gets reported as the boundary itself. `id`
            // (the youngest commit still >= since) becomes the boundary instead.
            let too_old = d.since.is_some_and(|since| {
                gix_object::FindExt::find(odb, &p, &mut pbuf)
                    .ok()
                    .and_then(|o| {
                        o.decode().ok().and_then(|dec| match dec {
                            gix_object::ObjectRef::Commit(c) => c.time().ok(),
                            _ => None,
                        })
                    })
                    .is_some_and(|t| t.seconds < since)
            });
            if cut.contains(&p) || too_old {
                boundary.push(id);
            } else {
                queue.push_back((p, depth + 1));
            }
        }
    }

    // A commit the client listed as a boundary is complete now if we reached it
    // and are not cutting there again.
    let boundary_set: HashSet<ObjectId> = boundary.iter().copied().collect();
    let unshallow = d
        .client_shallow
        .iter()
        .copied()
        .filter(|c| depth_of.contains_key(c) && !boundary_set.contains(c))
        .collect();

    boundary.sort();
    boundary.dedup();
    Ok(Shallow {
        commits: depth_of.into_keys().collect(),
        boundary,
        unshallow,
    })
}

/// Which of `targets` a commit walk from `tips` reaches — the `have` question, and the "is
/// this want ours" question for a commit. Stops the moment the last target is found, so an
/// up-to-date fetch (its haves ARE the tips) costs one lookup, and a client a few commits behind
/// pays for those few; only a have this repo has never seen walks every commit. The old answer
/// was the full object closure — O(repo) per fetch, for a question about commits.
pub(super) fn reachable_commits(
    odb: &gix_odb::Handle,
    tips: &[ObjectId],
    targets: &[ObjectId],
) -> Result<std::collections::HashSet<ObjectId>> {
    let mut want: std::collections::HashSet<ObjectId> = targets.iter().copied().collect();
    let mut found = std::collections::HashSet::new();
    let Peeled { commits, tags, .. } = peel_wants(odb, tips)?;
    for t in tags {
        if want.remove(&t) {
            found.insert(t);
        }
    }
    for info in gix_traverse::commit::Simple::new(commits, odb.clone()) {
        if want.is_empty() {
            break;
        }
        let id = info?.id;
        super::walked(1);
        if want.remove(&id) {
            found.insert(id);
        }
    }
    Ok(found)
}

/// What a list of wants splits into: commits (walkable), the tags passed through on the way to
/// them (sent as-is), and trees or blobs wanted directly (a promisor fetch; sent as-is).
///
/// Only commits can be walked, which is the whole reason for the split.
pub(super) struct Peeled {
    pub(super) commits: Vec<ObjectId>,
    pub(super) tags: Vec<ObjectId>,
    pub(super) leaves: Vec<ObjectId>,
}

pub(super) fn peel_wants(odb: &gix_odb::Handle, wants: &[ObjectId]) -> Result<Peeled> {
    let mut buf = Vec::new();
    let mut p = Peeled { commits: Vec::new(), tags: Vec::new(), leaves: Vec::new() };
    for w in wants {
        let mut id = *w;
        loop {
            match gix_object::FindExt::find(odb, &id, &mut buf)?.decode()? {
                gix_object::ObjectRef::Commit(_) => {
                    p.commits.push(id);
                    break;
                }
                gix_object::ObjectRef::Tag(t) => {
                    p.tags.push(id);
                    id = t.target();
                }
                _ => {
                    p.leaves.push(id);
                    break;
                }
            }
        }
    }
    Ok(p)
}

/// What one traversal of `wants`-minus-`haves` yields — computed once per fetch and shared
/// between the include-tag decision and the pack itself, because the walk is the expensive
/// half of serving a clone and used to run twice.
pub(crate) struct Range {
    /// Tags passed through on the way to the commits, then every commit in the range.
    pub(crate) ids: Vec<ObjectId>,
    /// Trees or blobs wanted directly (a promisor fetch) — kept apart because they are
    /// not filtered: the client asked for those exact objects.
    pub(crate) leaves: Vec<ObjectId>,
    /// The merge commits in the range, captured from the traversal's own parent list so the
    /// gix#2935 second pass (see `write_pack_range` in `pack.rs`) costs no re-decode of every
    /// commit.
    pub(crate) merges: Vec<ObjectId>,
    /// Parents outside the range — the commits the new history grows from. A push's
    /// connectivity check explains an unchanged subtree by their trees.
    pub(crate) boundary: Vec<ObjectId>,
}

/// The commits a fetch would send: reachable from `wants`, not from `haves`.
pub(crate) fn commit_range(
    odb: &gix_odb::Handle,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
) -> Result<Range> {
    let Peeled { commits, tags, leaves } = peel_wants(odb, &wants)?;
    let mut ids = tags;
    let mut merges = Vec::new();
    let mut parents = Vec::new();
    for info in gix_traverse::commit::Simple::new(commits, odb.clone()).hide(haves)? {
        let info = info?;
        if info.parent_ids.len() > 1 {
            merges.push(info.id);
        }
        parents.extend(info.parent_ids.iter().copied());
        ids.push(info.id);
    }
    let in_range: std::collections::HashSet<ObjectId> = ids.iter().copied().collect();
    let mut boundary: Vec<ObjectId> = parents.into_iter().filter(|p| !in_range.contains(p)).collect();
    boundary.sort();
    boundary.dedup();
    Ok(Range { ids, leaves, merges, boundary })
}

/// Count `ids` under `expansion`, then add a `TreeContents` pass for `leaves` — trees and blobs
/// the client wanted by id, which git expands whole. Deduped by id because each count call has
/// its own `seen` set and a repeated entry is a corrupt pack.
pub(super) fn counts_with_leaves(
    odb: &gix_odb::Handle,
    ids: Vec<ObjectId>,
    expansion: ObjectExpansion,
    leaves: Vec<ObjectId>,
    interrupt: &AtomicBool,
) -> Result<Vec<gix_pack::data::output::Count>> {
    let mut counts = super::pack::count_objects(odb, ids, expansion, interrupt)?;
    if !leaves.is_empty() {
        let mut seen: std::collections::HashSet<ObjectId> = counts.iter().map(|c| c.id).collect();
        counts.extend(
            super::pack::count_objects(odb, leaves, ObjectExpansion::TreeContents, interrupt)?
                .into_iter()
                .filter(|c| seen.insert(c.id)),
        );
    }
    Ok(counts)
}

#[cfg(test)]
mod parse_size_tests {
    use super::parse_size;

    #[test]
    fn parse_size_overflow_returns_none() {
        // 18014398509481984 * 1024^3 overflows u64; must not wrap to a small limit.
        assert_eq!(parse_size("18014398509481984g"), None);
    }

    #[test]
    fn parse_size_normal_values() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("10k"), Some(10 * 1024));
        assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
    }
}

//! Read-only views of a repo's object database: trees, blobs, history and diffs.
//!
//! Everything here is synchronous and takes a `gix_odb::Handle` — the odb is a local, already
//! materialised pack directory, so there is nothing to await.
use crate::Result;
use gix_hash::ObjectId;
use gix_object::{tree::EntryKind, FindExt};
use serde::Serialize;

/// Ceiling on a commit's unified diff. Half the api tier's 8 MiB `MAX_BODY`, so a truncated diff
/// still gets through the proxy rather than being built here and rejected there.
const MAX_DIFF: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub mode: u16,
    pub kind: String,
    pub oid: String,
    /// Blobs only; a tree has no meaningful size.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub oid: String,
    pub parents: Vec<String>,
    pub author: String,
    pub time: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Blob {
    pub oid: String,
    /// Named for what a JSON consumer receives: the bytes, base64-encoded. A blob is arbitrary
    /// binary, so it cannot go over JSON as a string.
    #[serde(rename = "bytes_base64", serialize_with = "as_base64")]
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

fn as_base64<S: serde::Serializer>(bytes: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
    use base64::Engine;
    s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// "What you asked for is not there" — an unknown oid, an unknown path, or an object of the wrong
/// kind. Everything else out of this module (a decode failure, an unreadable pack) is a bug or a
/// broken repo, and the HTTP layer answers 500 for it rather than hiding it behind a 404.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for NotFound {}

/// Whether this error means "not there" rather than "read failed".
pub fn is_not_found(e: &crate::Error) -> bool {
    e.downcast_ref::<NotFound>().is_some()
}

fn nf(msg: impl Into<String>) -> crate::Error {
    NotFound(msg.into()).into()
}

/// gix already separates a missing object from a failed read; keep that separation.
fn find_err(e: gix_object::find::existing::Error) -> crate::Error {
    match e {
        gix_object::find::existing::Error::NotFound { .. } => nf(e.to_string()),
        e => e.into(),
    }
}

fn find_obj_err(e: gix_object::find::existing_object::Error) -> crate::Error {
    match e {
        gix_object::find::existing_object::Error::NotFound { .. }
        | gix_object::find::existing_object::Error::ObjectKind { .. } => nf(e.to_string()),
        e => e.into(),
    }
}

fn peel_to_tree(odb: &gix_odb::Handle, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let data = odb.find(&oid, &mut buf).map_err(find_err)?;
    match data.kind {
        gix_object::Kind::Tree => Ok(oid),
        gix_object::Kind::Commit => Ok(gix_object::CommitRef::from_bytes(data.data, oid.kind())?.tree()),
        k => Err(nf(format!("{oid} is a {k}, not a commit or tree"))),
    }
}

/// Walk `path` from `tree`, returning the id it names. `""` is the tree itself.
fn resolve(odb: &gix_odb::Handle, tree: ObjectId, path: &str) -> Result<(ObjectId, EntryKind)> {
    let mut cur = (tree, EntryKind::Tree);
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if cur.1 != EntryKind::Tree {
            return Err(nf(format!("{seg}: parent is not a tree")));
        }
        let mut buf = Vec::new();
        let t = odb.find_tree(&cur.0, &mut buf).map_err(find_obj_err)?;
        let e = t
            .entries
            .iter()
            .find(|e| e.filename == seg)
            .ok_or_else(|| nf(format!("{path}: not found")))?;
        cur = (e.oid.to_owned(), e.mode.kind());
    }
    Ok(cur)
}

/// A directory's entries with no sizes. Sizes cost a header read each, and the
/// callers that only compare object ids — `last_changes`, and every interior node
/// of a recursive walk — were paying for every one of them.
fn entries_of(odb: &gix_odb::Handle, tree: ObjectId) -> Result<Vec<Entry>> {
    let mut buf = Vec::new();
    let mut out: Vec<Entry> = odb
        .find_tree(&tree, &mut buf)?
        .entries
        .iter()
        .map(|e| Entry {
            name: e.filename.to_string(),
            mode: e.mode.value(),
            kind: if e.mode.is_tree() { "tree".into() } else { "blob".into() },
            oid: e.oid.to_hex().to_string(),
            size: None,
        })
        .collect();
    out.sort_by(|a, b| (a.kind != "tree", &a.name).cmp(&(b.kind != "tree", &b.name)));
    Ok(out)
}

/// The tree a path names inside a commit, or `None` when the path is not there —
/// which is an ordinary answer when walking history, not an error.
fn tree_id_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str) -> Option<ObjectId> {
    let root = peel_to_tree(odb, oid).ok()?;
    match resolve(odb, root, path).ok()? {
        (id, EntryKind::Tree) => Some(id),
        _ => None,
    }
}

/// Fill in blob sizes from each object's header. Never inflates the object.
fn with_sizes(odb: &gix_odb::Handle, out: &mut [Entry]) {
    use gix_object::FindHeader;
    for e in out.iter_mut().filter(|e| e.kind == "blob") {
        if let Ok(id) = e.oid.parse::<ObjectId>() {
            e.size = odb.try_header(&id).ok().flatten().map(|h| h.size);
        }
    }
}

pub fn tree_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str) -> Result<Vec<Entry>> {
    let (id, kind) = resolve(odb, peel_to_tree(odb, oid)?, path)?;
    if kind != EntryKind::Tree {
        return Err(nf(format!("{path}: not a tree")));
    }
    let mut out = entries_of(odb, id)?;
    // A size lives in the object's HEADER — for a pack entry, in the few bytes
    // that introduce it. Reading the header never inflates the object, where
    // `find_blob` inflated every byte of every file in the directory to use only
    // its length: the whole file's cost paid for a number already written down.
    // Measured at 15.2ms -> 0.2ms on a directory with a large file in it.
    with_sizes(odb, &mut out);
    Ok(out)
}

pub fn blob_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str, cap: usize) -> Result<Blob> {
    let (id, kind) = resolve(odb, peel_to_tree(odb, oid)?, path)?;
    if kind == EntryKind::Tree {
        return Err(nf(format!("{path}: is a tree")));
    }
    let mut buf = Vec::new();
    let data = odb.find_blob(&id, &mut buf)?.data;
    let truncated = data.len() > cap;
    Ok(Blob {
        oid: id.to_hex().to_string(),
        bytes: data[..data.len().min(cap)].to_vec(),
        truncated,
    })
}

/// First-parent history from `from`, newest first, at most `n` entries.
pub fn log(odb: &gix_odb::Handle, from: ObjectId, n: usize) -> Result<Vec<Commit>> {
    let mut out = Vec::new();
    let mut next = Some(from);
    while let (Some(id), true) = (next, out.len() < n) {
        let mut buf = Vec::new();
        let c = odb.find_commit(&id, &mut buf).map_err(find_obj_err)?;
        next = c.parents().next();
        out.push(Commit {
            oid: id.to_hex().to_string(),
            parents: c.parents().map(|p| p.to_hex().to_string()).collect(),
            author: c.author()?.name.to_string(),
            time: c.time()?.seconds,
            message: c.message.to_string(),
        });
    }
    Ok(out)
}

/// Every blob under `path`, with its full path — the whole tree in one answer.
///
/// A caller that wants the shape of a repo (what it is written in, what paths
/// exist to jump to) needs all of it, and asking directory by directory turns one
/// local walk into a round trip per directory. This is a pure function of the
/// commit id, so it caches exactly as long as a tree does: forever.
///
/// `cap` bounds the result rather than the recursion depth — a repo with more
/// files than that returns what it reached, breadth-first, so the answer is a fair
/// sample rather than one deep branch.
pub fn files_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str, cap: usize) -> Result<Vec<Entry>> {
    let Some(root) = tree_id_at(odb, oid, path) else { return Ok(vec![]) };
    let mut out: Vec<Entry> = Vec::new();
    // Carries the tree ID, not the path: resolving `a/b/c` from the root for every
    // directory re-reads every tree above it, so a deep repo pays O(depth) per
    // directory for trees it has already decoded.
    let mut queue = std::collections::VecDeque::from([(path.to_string(), root)]);
    while let Some((dir, id)) = queue.pop_front() {
        if out.len() >= cap {
            break;
        }
        // A directory that cannot be read is skipped, not fatal: one unreadable
        // subtree must not lose the rest of the answer.
        let Ok(entries) = entries_of(odb, id) else { continue };
        for e in entries {
            let full = if dir.is_empty() { e.name.clone() } else { format!("{dir}/{}", e.name) };
            if e.kind == "tree" {
                if let Ok(child) = e.oid.parse::<ObjectId>() {
                    queue.push_back((full, child));
                }
            } else if out.len() < cap {
                out.push(Entry { name: full, ..e });
            }
        }
    }
    with_sizes(odb, &mut out);
    Ok(out)
}

/// The commit that last changed each entry of a directory.
///
/// The listing wants "what happened to this file last", which git does not store:
/// it is a walk of history comparing the entry against the same entry one commit
/// earlier. Done once for the whole directory rather than once per file — a walk
/// per row would read the same commits `n` times over.
///
/// Bounded by `budget` commits. History longer than that leaves the untouched
/// entries unattributed, which the caller renders as "no recent change" rather
/// than as a wrong one — a stale answer here is worse than an absent one.
pub fn last_changes(
    odb: &gix_odb::Handle,
    from: ObjectId,
    path: &str,
    budget: usize,
) -> Result<Vec<(String, Commit)>> {
    // What the directory looks like now. Anything not here is not being asked about.
    // A path that does not exist in an older commit is not an error — the
    // directory was added at some point — so a miss reads as "everything here is
    // new", which is exactly what it means.
    //
    // Sizes are never read here: this compares object ids, and a header read per
    // blob per commit is the whole walk's cost paid for a number nothing uses.
    let at = |oid: ObjectId| -> std::collections::BTreeMap<String, String> {
        tree_id_at(odb, oid, path)
            .and_then(|t| entries_of(odb, t).ok())
            .map(|es| es.into_iter().map(|e| (e.name, e.oid)).collect())
            .unwrap_or_default()
    };

    let mut pending: std::collections::BTreeSet<String> = at(from).into_keys().collect();
    let mut out = Vec::new();

    let mut cur = Some(from);
    let mut seen = 0;
    while let (Some(id), true) = (cur, seen < budget && !pending.is_empty()) {
        seen += 1;
        let c = match log(odb, id, 1)?.pop() {
            Some(c) => c,
            None => break,
        };
        let parent = c.parents.first().and_then(|p| p.parse::<ObjectId>().ok());
        let (now, before) = (at(id), parent.map(at).unwrap_or_default());

        // An entry changed here if its object id differs from the parent's — which
        // covers added (absent before) and modified alike.
        let changed: Vec<String> = pending
            .iter()
            .filter(|name| now.get(*name) != before.get(*name))
            .cloned()
            .collect();
        for name in changed {
            pending.remove(&name);
            out.push((name, c.clone()));
        }
        cur = parent;
    }
    Ok(out)
}

/// One side of a tree entry, owned so it outlives the odb buffer it was decoded from.
type Side = (ObjectId, bool);

/// Files that differ between `old` and `new`, as (path, old blob, new blob). Hand-rolled rather
/// than `gix-diff`: it is a merge on two sorted entry lists, and one less dependency to track.
fn changed_files(
    odb: &gix_odb::Handle,
    old: Option<ObjectId>,
    new: ObjectId,
    prefix: &str,
    out: &mut Vec<(String, Option<ObjectId>, Option<ObjectId>)>,
) -> Result<()> {
    let read = |id: Option<ObjectId>| -> Result<Vec<(String, Side)>> {
        let Some(id) = id else { return Ok(vec![]) };
        let mut buf = Vec::new();
        Ok(odb
            .find_tree(&id, &mut buf)?
            .entries
            .iter()
            .map(|e| (e.filename.to_string(), (e.oid.to_owned(), e.mode.is_tree())))
            .collect())
    };
    let mut sides: std::collections::BTreeMap<String, (Option<Side>, Option<Side>)> = Default::default();
    for (name, s) in read(old)? {
        sides.entry(name).or_default().0 = Some(s);
    }
    for (name, s) in read(Some(new))? {
        sides.entry(name).or_default().1 = Some(s);
    }
    for (name, (o, n)) in sides {
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        if o.as_ref().map(|s| s.0) == n.as_ref().map(|s| s.0) {
            continue;
        }
        let blob_id = |s: &Option<Side>| s.as_ref().filter(|s| !s.1).map(|s| s.0);
        let tree_id = |s: &Option<Side>| s.as_ref().filter(|s| s.1).map(|s| s.0);
        // A directory is never emitted itself: it is walked, against the other side's directory if
        // there is one and against the empty tree if there is not. A name that swapped between file
        // and directory therefore does both — walk one side, and emit the other side's blob.
        match (tree_id(&o), tree_id(&n)) {
            (None, None) => out.push((path, blob_id(&o), blob_id(&n))),
            (ot, nt) => {
                let empty = ObjectId::empty_tree(new.kind());
                changed_files(odb, ot, nt.unwrap_or(empty), &path, out)?;
                if let Some(b) = blob_id(&o) {
                    out.push((path, Some(b), None));
                } else if let Some(b) = blob_id(&n) {
                    out.push((path, None, Some(b)));
                }
            }
        }
    }
    Ok(())
}

/// A commit's signature, and the bytes it signs.
///
/// Git signs the commit object with its `gpgsig` header removed — so the payload is the raw
/// bytes with that header cut out, never a re-serialisation. Returning both means the verifier
/// never has to know how a commit is laid out.
pub struct Signed {
    /// The armoured signature: an OpenPGP block, or an SSH `SSHSIG` block.
    pub signature: String,
    /// Exactly the bytes the signature covers.
    pub payload: Vec<u8>,
    /// From the commit itself, for checking the signer is who the commit claims.
    pub author_email: String,
}

pub fn signature_of(odb: &gix_odb::Handle, oid: ObjectId) -> Result<Option<Signed>> {
    let mut buf = Vec::new();
    let data = odb.find(&oid, &mut buf).map_err(find_err)?;
    if data.kind != gix_object::Kind::Commit {
        return Err(nf(format!("{oid} is a {}, not a commit", data.kind)));
    }
    let commit = gix_object::CommitRef::from_bytes(data.data, oid.kind())?;
    // ponytail: sha1 repos only — a sha256-object repo signs under `gpgsig-sha256`, which reads as
    // unsigned here (fail closed); look for both names when sha256 repos are supported.
    let Some(sig) = commit.extra_headers().find("gpgsig") else {
        return Ok(None);
    };
    let signature = sig.to_string();
    let author_email = commit.author().map(|a| a.email.to_string()).unwrap_or_default();
    Ok(Some(Signed { signature, payload: without_gpgsig(data.data), author_email }))
}

/// The raw commit with the `gpgsig` header and its continuation lines cut out — exactly what git
/// hashed when it signed. Cut, not re-serialised: gix normalises what it parses (a `-0000` zone
/// comes back `+0000`), and a payload that is not byte-for-byte the original makes a perfectly
/// good signature read as invalid.
fn without_gpgsig(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut rest = raw;
    let mut in_sig = false;
    while !rest.is_empty() {
        let end = rest.iter().position(|&b| b == b'\n').map_or(rest.len(), |p| p + 1);
        let line = &rest[..end];
        // The blank line ends the headers; the message after it travels verbatim.
        if line == b"\n" {
            out.extend_from_slice(rest);
            break;
        }
        if line.starts_with(b"gpgsig ") {
            // A malformed commit with two `gpgsig` headers loses both runs while the signature
            // above is taken from the first: the payload then mismatches, so it reads Invalid —
            // never a false Valid, which is the only direction that matters here.
            in_sig = true;
        } else if in_sig && line.starts_with(b" ") {
            // a continuation line of the signature
        } else {
            in_sig = false;
            out.extend_from_slice(line);
        }
        rest = &rest[end..];
    }
    out
}

/// What a diff says in place of a binary file's contents. The web reads this
/// exact line, so it is a constant rather than a spelling repeated in two repos.
pub const BINARY_MARKER: &str = "Binary file not shown";

/// What a diff says for a blob past `MAX_DIFF`. Decided from the object header, so the blob is
/// never inflated to learn it cannot be shown.
pub const TOO_LARGE_MARKER: &str = "File too large to diff";

/// Git's own heuristic: a NUL in the first 8000 bytes means binary. Cheap, and
/// wrong only for text that contains a NUL, which is not text.
fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0)
}

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

/// What a proposed change contains: the commits on `head` that `base` does not
/// have, and one diff of the whole thing.
///
/// The diff is taken from the MERGE BASE, not from the base tip. Diffing against
/// the tip would attribute every commit that landed on base since the branch left
/// it to this change — the reviewer would be reading other people's work as if it
/// were the author's.
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub base: String,
    pub head: String,
    /// `None` when the two histories are unrelated within the walk's budget.
    pub merge_base: Option<String>,
    /// Whether `base` can be moved to `head` without a merge commit.
    pub fast_forward: bool,
    pub commits: Vec<Commit>,
    pub diff: String,
}

pub fn compare(
    odb: &gix_odb::Handle,
    base: ObjectId,
    head: ObjectId,
    max_commits: usize,
) -> Result<Comparison> {
    const BUDGET: usize = 50_000;
    let mb = merge_base(odb, base, head, BUDGET);

    // Commits on head that base does not have. `hide` is exactly this question,
    // and asking it of the traversal is cheaper than walking both and subtracting.
    let commits = gix_traverse::commit::Simple::new(Some(head), odb.clone())
        .hide(Some(base))
        .map_err(|e| crate::err(e.to_string()))?
        .take(max_commits)
        .filter_map(|i| i.ok())
        .map(|i| commit_meta(odb, i.id))
        .collect::<Result<Vec<_>>>()?;

    let diff = match mb {
        Some(from) => diff_trees(odb, Some(from), head)?,
        // Unrelated histories: there is no shared point to diff from, and showing
        // the whole of `head` as an addition would be a lie about what changed.
        None => String::new(),
    };

    Ok(Comparison {
        base: base.to_hex().to_string(),
        head: head.to_hex().to_string(),
        merge_base: mb.map(|o| o.to_hex().to_string()),
        fast_forward: mb == Some(base),
        commits,
        diff,
    })
}

/// One commit's metadata, without its diff.
fn commit_meta(odb: &gix_odb::Handle, id: ObjectId) -> Result<Commit> {
    log(odb, id, 1)?.pop().ok_or_else(|| nf("no such commit"))
}

/// A commit and a unified diff of it against its first parent (against the empty tree for a root
/// commit).
pub fn commit(odb: &gix_odb::Handle, oid: ObjectId) -> Result<(Commit, String)> {
    let c = log(odb, oid, 1)?.pop().ok_or_else(|| nf("no such commit"))?;
    let tree = peel_to_tree(odb, oid)?;
    let parent_tree = match c.parents.first() {
        Some(p) => Some(peel_to_tree(odb, p.parse()?)?),
        None => None,
    };
    let diff = diff_trees_inner(odb, parent_tree, tree)?;
    Ok((c, diff))
}

/// A unified diff between two COMMITS, from `from`'s tree to `to`'s.
fn diff_trees(odb: &gix_odb::Handle, from: Option<ObjectId>, to: ObjectId) -> Result<String> {
    let to_tree = peel_to_tree(odb, to)?;
    let from_tree = match from {
        Some(f) => Some(peel_to_tree(odb, f)?),
        None => None,
    };
    diff_trees_inner(odb, from_tree, to_tree)
}

fn diff_trees_inner(
    odb: &gix_odb::Handle,
    parent_tree: Option<ObjectId>,
    tree: ObjectId,
) -> Result<String> {
    let mut files = Vec::new();
    changed_files(odb, parent_tree, tree, "", &mut files)?;
    let mut diff = String::new();
    for (path, old, new) in files {
        // ponytail: 4 MiB ceiling on the whole diff, checked between files; a single file past
        // the ceiling is caught by the header read below, before anything is inflated. A commit
        // that touches a thousand large blobs would otherwise decompress all of them into one
        // String on the git node — the same memory cliff the push path has a cap for. Stream it
        // per file if a client ever needs the full text of a commit this large.
        if diff.len() >= MAX_DIFF {
            diff.push_str("\n[diff truncated]\n");
            break;
        }
        // From the header, never by inflating: a blob past the ceiling cannot be shown anyway,
        // and reading it to find that out is the memory cliff the ceiling exists to avoid.
        let too_big = |id: Option<ObjectId>| -> bool {
            use gix_object::FindHeader;
            // Keep-biased: a header that cannot be read is treated as too big, because the
            // alternative is inflating an object of unknown size to find out.
            id.is_some_and(|id| {
                odb.try_header(&id)
                    .ok()
                    .flatten()
                    .is_none_or(|h| h.size > MAX_DIFF as u64)
            })
        };
        if too_big(old) || too_big(new) {
            diff.push_str(&format!("--- a/{path}\n+++ b/{path}\n{TOO_LARGE_MARKER}\n"));
            continue;
        }
        let bytes = |id: Option<ObjectId>| -> Result<Vec<u8>> {
            Ok(match id {
                Some(id) => {
                    let mut b = Vec::new();
                    odb.find_blob(&id, &mut b)?.data.to_vec()
                }
                None => Vec::new(),
            })
        };
        let (ab, bb) = (bytes(old)?, bytes(new)?);

        // Binary, by git's own rule: a NUL byte near the start. Diffing it as
        // lossy UTF-8 produces pages of replacement characters that say nothing
        // about what changed and bury every real hunk in the commit.
        if is_binary(&ab) || is_binary(&bb) {
            diff.push_str(&format!("--- a/{path}\n+++ b/{path}\n{BINARY_MARKER}\n"));
            continue;
        }
        let (a, b) = (
            String::from_utf8_lossy(&ab).into_owned(),
            String::from_utf8_lossy(&bb).into_owned(),
        );
        // For display, not for `git apply`: no `diff --git` header, and an added or deleted file
        // still names `a/path` and `b/path` rather than /dev/null.
        diff.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
        let input = imara_diff::intern::InternedInput::new(a.as_str(), b.as_str());
        diff.push_str(&imara_diff::diff(
            imara_diff::Algorithm::Histogram,
            &input,
            imara_diff::UnifiedDiffBuilder::new(&input),
        ));
    }
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::without_gpgsig;

    /// git signed the raw bytes minus the `gpgsig` header; a payload rebuilt by re-serialising is
    /// not those bytes whenever gix normalises something it parsed — here the `-0000` zone.
    #[test]
    fn signature_payload_is_cut_from_the_raw_bytes() {
        let raw: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author t <t@t> 0 -0000\n\
committer t <t@t> 0 -0000\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n -----END PGP SIGNATURE-----\n\
\n\
msg\n";
        let want: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author t <t@t> 0 -0000\n\
committer t <t@t> 0 -0000\n\
\n\
msg\n";
        assert_eq!(without_gpgsig(raw), want);

        // A continuation line under a header that is not `gpgsig` stays.
        let other: &[u8] = b"tree x\nmergetag object y\n z\n\nmsg\n";
        assert_eq!(without_gpgsig(other), other);

        // Only headers are cut: the message is copied verbatim, `gpgsig ` line and all.
        let body: &[u8] = b"tree x\n\ngpgsig not a header here\n";
        assert_eq!(without_gpgsig(body), body);

        // Headers with no blank line and no message: cut, and no panic on the unterminated end.
        assert_eq!(without_gpgsig(b"tree x\ngpgsig sig\n more"), b"tree x\n");

        // The re-serialising approach this replaces cannot produce `want`.
        use gix_object::WriteTo;
        let parsed = gix_object::CommitRef::from_bytes(raw, gix_hash::Kind::Sha1).unwrap();
        if let Ok(mut owned) = parsed.to_owned() {
            owned.extra_headers.retain(|(name, _)| name.as_slice() != b"gpgsig");
            let mut rebuilt = Vec::new();
            owned.write_to(&mut rebuilt).unwrap();
            assert_ne!(rebuilt, want, "if these are equal the fixture no longer proves anything");
        }
    }

    #[test]
    fn a_commit_without_a_signature_is_unchanged() {
        let raw: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\nmsg\n";
        assert_eq!(without_gpgsig(raw), raw);
    }
}

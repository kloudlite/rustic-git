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

pub fn tree_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str) -> Result<Vec<Entry>> {
    let (id, kind) = resolve(odb, peel_to_tree(odb, oid)?, path)?;
    if kind != EntryKind::Tree {
        return Err(nf(format!("{path}: not a tree")));
    }
    let mut buf = Vec::new();
    let entries = odb.find_tree(&id, &mut buf)?.entries;
    let ids: Vec<ObjectId> = entries.iter().map(|e| e.oid.to_owned()).collect();
    let mut out: Vec<Entry> = entries
        .iter()
        .map(|e| Entry {
            name: e.filename.to_string(),
            mode: e.mode.value(),
            kind: if e.mode.is_tree() { "tree".into() } else { "blob".into() },
            oid: e.oid.to_hex().to_string(),
            size: None,
        })
        .collect();
    // ponytail: sizes decompress every blob; read pack entry headers if listings get slow
    for (e, id) in out.iter_mut().zip(ids).filter(|(e, _)| e.kind == "blob") {
        let mut b = Vec::new();
        e.size = Some(odb.find_blob(&id, &mut b)?.data.len() as u64);
    }
    out.sort_by(|a, b| (a.kind != "tree", &a.name).cmp(&(b.kind != "tree", &b.name)));
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

/// A commit and a unified diff of it against its first parent (against the empty tree for a root
/// commit).
pub fn commit(odb: &gix_odb::Handle, oid: ObjectId) -> Result<(Commit, String)> {
    let c = log(odb, oid, 1)?.pop().ok_or_else(|| nf("no such commit"))?;
    let tree = peel_to_tree(odb, oid)?;
    let parent_tree = match c.parents.first() {
        Some(p) => Some(peel_to_tree(odb, p.parse()?)?),
        None => None,
    };
    let mut files = Vec::new();
    changed_files(odb, parent_tree, tree, "", &mut files)?;
    let mut diff = String::new();
    for (path, old, new) in files {
        // ponytail: 4 MiB ceiling on the whole diff, checked between files. A commit that touches
        // a thousand large blobs would otherwise decompress all of them into one String on the git
        // node — the same memory cliff the push path has a cap for. Stream it per file if a client
        // ever needs the full text of a commit this large.
        if diff.len() >= MAX_DIFF {
            diff.push_str("\n[diff truncated]\n");
            break;
        }
        // ponytail: lossy UTF-8 diff; detect binary when someone complains
        let text = |id: Option<ObjectId>| -> Result<String> {
            Ok(match id {
                Some(id) => {
                    let mut b = Vec::new();
                    String::from_utf8_lossy(odb.find_blob(&id, &mut b)?.data).to_string()
                }
                None => String::new(),
            })
        };
        let (a, b) = (text(old)?, text(new)?);
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
    Ok((c, diff))
}

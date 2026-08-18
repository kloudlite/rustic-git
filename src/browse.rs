//! Read-only views of a repo's object database: trees, blobs, history and diffs.
//!
//! Everything here is synchronous and takes a `gix_odb::Handle` — the odb is a local, already
//! materialised pack directory, so there is nothing to await.
use crate::{err, Result};
use gix_hash::ObjectId;
use gix_object::{tree::EntryKind, FindExt};
use serde::Serialize;

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

fn peel_to_tree(odb: &gix_odb::Handle, oid: ObjectId) -> Result<ObjectId> {
    let mut buf = Vec::new();
    let data = odb.find(&oid, &mut buf)?;
    match data.kind {
        gix_object::Kind::Tree => Ok(oid),
        gix_object::Kind::Commit => Ok(gix_object::CommitRef::from_bytes(data.data, oid.kind())?.tree()),
        k => Err(err(format!("{oid} is a {k}, not a commit or tree"))),
    }
}

/// Walk `path` from `tree`, returning the id it names. `""` is the tree itself.
fn resolve(odb: &gix_odb::Handle, tree: ObjectId, path: &str) -> Result<(ObjectId, EntryKind)> {
    let mut cur = (tree, EntryKind::Tree);
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if cur.1 != EntryKind::Tree {
            return Err(err(format!("{seg}: parent is not a tree")));
        }
        let mut buf = Vec::new();
        let t = odb.find_tree(&cur.0, &mut buf)?;
        let e = t
            .entries
            .iter()
            .find(|e| e.filename == seg)
            .ok_or_else(|| err(format!("{path}: not found")))?;
        cur = (e.oid.to_owned(), e.mode.kind());
    }
    Ok(cur)
}

pub fn tree_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str) -> Result<Vec<Entry>> {
    let (id, kind) = resolve(odb, peel_to_tree(odb, oid)?, path)?;
    if kind != EntryKind::Tree {
        return Err(err(format!("{path}: not a tree")));
    }
    let mut buf = Vec::new();
    let mut out: Vec<Entry> = odb
        .find_tree(&id, &mut buf)?
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
    for e in out.iter_mut().filter(|e| e.kind == "blob") {
        let id: ObjectId = e.oid.parse()?;
        let mut b = Vec::new();
        e.size = Some(odb.find_blob(&id, &mut b)?.data.len() as u64);
    }
    out.sort_by(|a, b| (a.kind != "tree", &a.name).cmp(&(b.kind != "tree", &b.name)));
    Ok(out)
}

pub fn blob_at(odb: &gix_odb::Handle, oid: ObjectId, path: &str, cap: usize) -> Result<Blob> {
    let (id, kind) = resolve(odb, peel_to_tree(odb, oid)?, path)?;
    if kind == EntryKind::Tree {
        return Err(err(format!("{path}: is a tree")));
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
        let c = odb.find_commit(&id, &mut buf)?;
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
        let is_tree = |s: &Option<Side>| s.as_ref().is_some_and(|s| s.1);
        let blob_id = |s: &Option<Side>| s.as_ref().filter(|s| !s.1).map(|s| s.0);
        match (is_tree(&o), &n) {
            // A directory on the new side: recurse rather than emit it. If the old side was a
            // blob under the same name it shows up as a deletion inside the recursion's parent —
            // close enough for a browse view.
            (_, Some(s)) if s.1 => {
                changed_files(odb, o.as_ref().filter(|s| s.1).map(|s| s.0), s.0, &path, out)?;
            }
            // A directory that disappeared: everything under it is a deletion.
            (true, _) => changed_files_deleted(odb, o.as_ref().unwrap().0, &path, out)?,
            _ => out.push((path, blob_id(&o), blob_id(&n))),
        }
    }
    Ok(())
}

fn changed_files_deleted(
    odb: &gix_odb::Handle,
    tree: ObjectId,
    prefix: &str,
    out: &mut Vec<(String, Option<ObjectId>, Option<ObjectId>)>,
) -> Result<()> {
    let mut buf = Vec::new();
    let entries: Vec<_> = odb
        .find_tree(&tree, &mut buf)?
        .entries
        .iter()
        .map(|e| (e.filename.to_string(), e.oid.to_owned(), e.mode.is_tree()))
        .collect();
    for (name, oid, is_tree) in entries {
        let path = format!("{prefix}/{name}");
        if is_tree {
            changed_files_deleted(odb, oid, &path, out)?;
        } else {
            out.push((path, Some(oid), None));
        }
    }
    Ok(())
}

/// A commit and a unified diff of it against its first parent (against the empty tree for a root
/// commit).
pub fn commit(odb: &gix_odb::Handle, oid: ObjectId) -> Result<(Commit, String)> {
    let c = log(odb, oid, 1)?.pop().ok_or_else(|| err("no such commit"))?;
    let tree = peel_to_tree(odb, oid)?;
    let parent_tree = match c.parents.first() {
        Some(p) => Some(peel_to_tree(odb, p.parse()?)?),
        None => None,
    };
    let mut files = Vec::new();
    changed_files(odb, parent_tree, tree, "", &mut files)?;
    let mut diff = String::new();
    for (path, old, new) in files {
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

//! Putting an object we made into a repo.
//!
//! Everything else here only ever RECEIVES objects — a push arrives as a pack,
//! gets indexed, and is uploaded. Nothing constructs one. Merging does: a squash
//! or a merge commit is a commit that did not exist until the server made it.
//!
//! The route is deliberately the same one a push takes. A new object is written
//! into a one-object pack, indexed by the same `Bundle::write_to_directory` that
//! validates every push, and uploaded by the same `upload_pack_files`. That means
//! an object we invent is stored, verified and replicated exactly like one a
//! client sent — no second path to storage, and no second set of bugs.

use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use gix_object::WriteTo;
use std::sync::atomic::AtomicBool;

/// Objects made in memory, waiting to be written together.
///
/// A three-way merge produces new blobs (merged file contents) and new trees
/// (every directory on the path to a changed file), and only then the commit. All
/// of them have to land, and landing them one pack at a time would leave a repo
/// holding a tree whose blobs are missing if anything failed in between.
///
/// So ids are computed as objects are made — a git id IS the hash of the bytes,
/// so nothing has to be stored to know it — and everything is written in one pack
/// at the end. Either the whole merge lands or none of it does.
#[derive(Default)]
pub struct Staging {
    objects: Vec<(gix_object::Kind, Vec<u8>)>,
}

impl Staging {
    /// Stage one object and return the id it will have.
    pub fn add(&mut self, kind: gix_object::Kind, body: Vec<u8>) -> Result<ObjectId> {
        let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, kind, &body)
            .map_err(|e| err(e.to_string()))?;
        self.objects.push((kind, body));
        Ok(oid)
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Write everything staged into `repo`, in one pack.
    pub async fn write(self, store: &Store, repo: &Repo) -> Result<()> {
        if self.objects.is_empty() {
            return Ok(());
        }
        write_pack_of_objects(store, repo, &self.objects).await
    }
}

/// What a caller has to say to make a commit.
///
/// Deliberately not `gix_object::Commit`: the api tier should be able to ask for
/// a squash without learning gitoxide's types, and keeping the git shapes on this
/// side of the wall means the merge strategies read as what they mean.
pub struct NewCommit {
    /// The tree the commit points at — for a squash or a merge commit, the head
    /// branch's tree, since the content being landed is exactly what is on it.
    pub tree: ObjectId,
    /// One parent squashes; two make a merge commit.
    pub parents: Vec<ObjectId>,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    /// Seconds since the epoch. Passed in rather than read from the clock so the
    /// same request twice produces the same commit id.
    pub time: i64,
}

/// Write one commit into `repo` and return its id.
///
/// The id is computed from the object's bytes, so it is known before the write —
/// which is what makes this safe to retry: writing the same commit twice produces
/// the same id and the second pack is redundant rather than wrong.
pub async fn write_commit(store: &Store, repo: &Repo, new: NewCommit) -> Result<ObjectId> {
    let who = gix_actor::Signature {
        name: new.author_name.into(),
        email: new.author_email.into(),
        time: gix_object::date::Time::new(new.time, 0),
    };
    let commit = gix_object::Commit {
        tree: new.tree,
        parents: new.parents.into(),
        author: who.clone(),
        committer: who,
        encoding: None,
        message: new.message.into(),
        extra_headers: Vec::new(),
    };

    let mut body = Vec::new();
    commit.write_to(&mut body)?;
    let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Commit, &body)
        .map_err(|e| err(e.to_string()))?;

    // Already there — a retry, or two people merging the same thing at once.
    if repo.odb().is_ok_and(|odb| gix_object::Find::try_find(&odb, &oid, &mut Vec::new()).is_ok_and(|o| o.is_some())) {
        return Ok(oid);
    }

    write_pack_of_objects(store, repo, &[(gix_object::Kind::Commit, body)]).await?;
    Ok(oid)
}

/// Write a set of objects into `repo` as one pack, through the push path.
///
/// Indexed by the same `Bundle::write_to_directory` that validates every push, so
/// a malformed object fails here rather than becoming a ref nobody can read.
async fn write_pack_of_objects(
    store: &Store,
    repo: &Repo,
    objects: &[(gix_object::Kind, Vec<u8>)],
) -> Result<()> {
    std::fs::create_dir_all(&repo.pack_dir)?;
    // Named after the content it carries: two writers racing on the same merge
    // produce the same file rather than corrupting each other's.
    let mut naming = gix_hash::hasher(gix_hash::Kind::Sha1);
    for (_, body) in objects {
        naming.update(body);
    }
    let stamp = naming.try_finalize().map_err(|e| err(e.to_string()))?;
    let pack_path = repo.pack_dir.join(format!("incoming-{}.pack", stamp.to_hex()));
    write_object_pack(objects, &pack_path)?;

    let odb = repo.odb()?;
    let mut reader = std::io::BufReader::new(std::fs::File::open(&pack_path)?);
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut reader,
        Some(&repo.pack_dir),
        &mut gix_features::progress::Discard,
        &AtomicBool::new(false),
        Some(odb),
        gix_pack::bundle::write::Options {
            thread_limit: None,
            iteration_mode: gix_pack::data::input::Mode::Verify,
            index_version: gix_pack::index::Version::V2,
            object_hash: gix_hash::Kind::Sha1,
            alloc_limit_bytes: Some(1024 * 1024 * 1024),
            compression: Default::default(),
        },
    )?;
    if let Some(k) = outcome.keep_path {
        let _ = std::fs::remove_file(k);
    }
    let _ = std::fs::remove_file(&pack_path);
    let (Some(data), Some(index)) = (outcome.data_path, outcome.index_path) else {
        return Err(err("the new objects produced no pack"));
    };
    store.upload_pack_files(repo, &data, &index).await
}

/// A pack holding the given objects, written by hand.
///
/// `gix-pack`'s writer builds from an odb the objects are already in, which is
/// the wrong way round here — the object does not exist yet. A single-object pack
/// is a 12-byte header, one zlib-compressed entry and a trailing checksum, so it
/// is written directly rather than by putting the object somewhere first just to
/// read it back.
fn write_object_pack(
    objects: &[(gix_object::Kind, Vec<u8>)],
    path: &std::path::Path,
) -> Result<()> {
    use std::io::Write;
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes()); // version
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());

    for (kind, body) in objects {
        // Entry header: type in bits 4-6, size in a base-128 varint whose FIRST
        // group is only 4 bits wide — the quirk that makes this format easy to
        // get wrong.
        let type_bits: u8 = match kind {
            gix_object::Kind::Commit => 1,
            gix_object::Kind::Tree => 2,
            gix_object::Kind::Blob => 3,
            gix_object::Kind::Tag => 4,
        };
        let mut size = body.len() as u64;
        let mut byte = (type_bits << 4) | (size as u8 & 0x0f);
        size >>= 4;
        while size > 0 {
            out.push(byte | 0x80);
            byte = (size as u8) & 0x7f;
            size >>= 7;
        }
        out.push(byte);

        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(body)?;
        out.extend_from_slice(&z.finish()?);
    }

    // The trailer is the SHA-1 of everything before it.
    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&out);
    let checksum = hasher.try_finalize().map_err(|e| err(e.to_string()))?;
    out.extend_from_slice(checksum.as_bytes());

    std::fs::write(path, &out)?;
    Ok(())
}

// ── patches ─────────────────────────────────────────────────────────────────

/// What to do to one path in a patch.
pub enum Change {
    /// Write these bytes there, creating the file or replacing what is there.
    Upsert {
        content: Vec<u8>,
        /// `None` keeps the mode the file already has, so editing a script does
        /// not quietly drop its executable bit. New files default to non-executable.
        executable: Option<bool>,
    },
    Delete,
}

/// Build the tree that results from applying `changes` to `base`.
///
/// A patch is ONE commit over many files, so the tree is rebuilt once for the
/// whole set rather than once per file — every directory on the way to a change
/// would otherwise be re-encoded once for each file inside it.
///
/// The new blobs and trees are staged, not written: the caller writes them with
/// the commit, so a failure leaves no tree whose blobs are missing.
pub fn apply_changes(
    odb: &(impl gix_object::FindExt + gix_object::Find),
    base: Option<ObjectId>,
    changes: &std::collections::BTreeMap<String, Change>,
    staging: &mut Staging,
) -> Result<ObjectId> {
    use gix_object::tree::EntryKind;

    if changes.is_empty() {
        return Err(err("a commit needs at least one change"));
    }

    let root = match base {
        Some(oid) => {
            let mut buf = Vec::new();
            gix_object::FindExt::find_tree(odb, &oid, &mut buf)
                .map_err(|e| err(format!("reading the base tree: {e}")))?
                .into()
        }
        None => gix_object::Tree::empty(),
    };
    let mut editor = gix_object::tree::Editor::new(root, odb, gix_hash::Kind::Sha1);

    for (path, change) in changes {
        let parts = split_path(path)?;
        match change {
            Change::Upsert { content, executable } => {
                // The mode the path already has, so editing a script keeps its
                // executable bit. A path that is a symlink or a submodule is
                // refused: writing bytes over either is a corrupt tree, not an edit.
                let kind = match editor.get(parts.iter()).map(|e| e.mode.kind()) {
                    Some(EntryKind::Link) => return Err(err(format!("{path} is a symbolic link"))),
                    Some(EntryKind::Commit) => return Err(err(format!("{path} is a submodule"))),
                    Some(EntryKind::Tree) => return Err(err(format!("{path} is a directory"))),
                    existing => match executable {
                        Some(true) => EntryKind::BlobExecutable,
                        Some(false) => EntryKind::Blob,
                        None if existing == Some(EntryKind::BlobExecutable) => {
                            EntryKind::BlobExecutable
                        }
                        None => EntryKind::Blob,
                    },
                };
                let blob = staging.add(gix_object::Kind::Blob, content.clone())?;
                editor
                    .upsert(parts.iter(), kind, blob)
                    .map_err(|e| err(format!("{path}: {e}")))?;
            }
            // `remove_leaf`, not `remove`: deleting a path that turned out to be a
            // directory would take everything under it with it, which is not what
            // "delete this file" asked for.
            Change::Delete => {
                if editor.get(parts.iter()).is_none() {
                    return Err(err(format!("{path} is not in this branch")));
                }
                editor
                    .remove_leaf(parts.iter())
                    .map_err(|e| err(format!("{path}: {e}")))?;
            }
        }
    }

    editor.write(|tree| {
        let mut body = Vec::new();
        tree.write_to(&mut body)?;
        staging.add(gix_object::Kind::Tree, body)
    })
}

/// A path's components, refused unless every one of them is a name git will
/// store and a client will check out.
///
/// `..` is the one that matters: a tree entry is a NAME, so a component that
/// means "the parent" cannot be stored — but a client checking the tree out
/// resolves it against the filesystem, which is a write outside the worktree.
fn split_path(path: &str) -> Result<Vec<&str>> {
    if path.len() > 4096 {
        return Err(err("path is too long"));
    }
    let parts: Vec<&str> = path.split('/').collect();
    for p in &parts {
        let bad = p.is_empty()
            || *p == "."
            || *p == ".."
            || p.eq_ignore_ascii_case(".git")
            || p.contains('\\')
            || p.bytes().any(|b| b < 0x20 || b == 0x7f);
        if bad {
            return Err(err(format!("{path} is not a valid path")));
        }
    }
    Ok(parts)
}

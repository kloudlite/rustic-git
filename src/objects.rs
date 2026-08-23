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
        write_pack_of_objects(store, repo, self.objects).await
    }
}

/// What came out of merging two trees.
pub enum TreeMerge {
    /// The merged tree, and the blobs and trees that back it. NOTHING is stored yet — the caller
    /// writes `staging` if and only if it decides to land the merge.
    Clean { tree: ObjectId, staging: Staging },
    /// Paths a person has to decide, sorted and deduplicated. Nothing was invented.
    Conflicts(Vec<String>),
}

/// The sentence a person reads when a merge conflicts. Capped: the point is to say where to look,
/// not to paste a build log into a pull request.
pub fn conflict_detail(paths: &[String]) -> String {
    let shown = paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    let more = paths.len().saturating_sub(5);
    let tail = if more > 0 { format!(" +{more} more") } else { String::new() };
    format!("conflicts in: {shown}{tail}")
}

/// Merge `ours` and `theirs` against their common ancestor `base`, in memory.
///
/// This is git's three-way merge, done by `gix_merge::tree`: same-file edits on different lines
/// combine, edits to the same lines do not. It reads from the odb and writes nothing, which is
/// what lets the mergeability check run exactly the same merge as the merge itself — a dry run is
/// this call with the `Staging` dropped, so "this will merge cleanly" cannot drift from what
/// happens when someone presses the button.
///
/// `ours`/`theirs` label the sides inside any conflict markers that end up in a file.
pub fn merge_trees(
    odb: &impl gix_object::FindObjectOrHeader,
    base: ObjectId,
    ours: ObjectId,
    theirs: ObjectId,
    our_label: &str,
    their_label: &str,
) -> Result<TreeMerge> {
    use gix_object::bstr::ByteSlice;

    // No worktree exists on this server, so attributes can only come from the objects themselves
    // and every filter is the identity. Two stacks because the two platforms each own one.
    let attr_stack = || {
        gix_worktree::Stack::new(
            std::path::PathBuf::new(),
            gix_worktree::stack::State::AttributesStack(gix_worktree::stack::state::Attributes::new(
                Default::default(),
                None,
                gix_worktree::stack::state::attributes::Source::IdMapping,
                Default::default(),
            )),
            gix_worktree::glob::pattern::Case::Sensitive,
            Vec::new(),
            Vec::new(),
        )
    };
    let mut blob_merge = gix_merge::blob::Platform::new(
        gix_merge::blob::Pipeline::new(Default::default(), gix_filter::Pipeline::default(), Default::default()),
        gix_merge::blob::pipeline::Mode::ToGit,
        attr_stack(),
        Vec::new(),
        Default::default(),
    );
    let mut diff_cache = gix_diff::blob::Platform::new(
        Default::default(),
        gix_diff::blob::Pipeline::new(Default::default(), gix_filter::Pipeline::default(), Vec::new(), Default::default()),
        gix_diff::blob::pipeline::Mode::ToGit,
        attr_stack(),
    );
    let mut diff_state = gix_diff::tree::State::default();

    let mut staging = Staging::default();
    let mut outcome = gix_merge::tree(
        &base,
        &ours,
        &theirs,
        gix_merge::blob::builtin_driver::text::Labels {
            ancestor: Some(b"base".as_bstr()),
            current: Some(our_label.as_bytes().as_bstr()),
            other: Some(their_label.as_bytes().as_bstr()),
        },
        odb,
        |content| staging.add(gix_object::Kind::Blob, content.to_vec()),
        &mut diff_state,
        &mut diff_cache,
        &mut blob_merge,
        gix_merge::tree::Options {
            // Anything gix cannot decide stays a conflict for a person, rather than being resolved
            // by picking a side here — silently dropping one side of a merge is the one outcome
            // nobody can see happened.
            tree_conflicts: None,
            ..Default::default()
        },
    )
    .map_err(|e| err(e.to_string()))?;

    let how = gix_merge::tree::TreatAsUnresolved::default();
    if outcome.has_unresolved_conflicts(how) {
        let paths: std::collections::BTreeSet<String> = outcome
            .conflicts
            .iter()
            .filter(|c| c.is_unresolved(how))
            .map(|c| c.changes_in_resolution().0.location().to_string())
            .collect();
        return Ok(TreeMerge::Conflicts(paths.into_iter().collect()));
    }

    // The editor hands back every tree it changed, innermost first, and the root last. Each one is
    // staged rather than written, so a merge that turns out to be refused later leaves no trace.
    let mut body = Vec::new();
    let tree = outcome
        .tree
        .write(|t| {
            body.clear();
            t.write_to(&mut body)?;
            staging.add(gix_object::Kind::Tree, body.clone())
        })
        .map_err(|e: crate::Error| e)?;
    Ok(TreeMerge::Clean { tree, staging })
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

    write_pack_of_objects(store, repo, vec![(gix_object::Kind::Commit, body)]).await?;
    Ok(oid)
}

/// Write a set of objects into `repo` as one pack, through the push path.
///
/// Indexed by the same `Bundle::write_to_directory` that validates every push, so
/// a malformed object fails here rather than becoming a ref nobody can read. The
/// indexing is CPU work (zlib, SHA-1) and runs on a blocking thread: the api tier
/// awaits this from a request handler, and a merge stalling every other request on
/// that worker thread is how "merge" shows up as latency on unrelated pages.
async fn write_pack_of_objects(
    store: &Store,
    repo: &Repo,
    objects: Vec<(gix_object::Kind, Vec<u8>)>,
) -> Result<()> {
    let r = repo.clone();
    let (data, index) = tokio::task::spawn_blocking(move || index_objects(&r, &objects)).await??;
    store.upload_pack_files(repo, &data, &index).await
}

/// Where this call's temp pack goes. Per process and call, never by content: two merges of the
/// same staged content would otherwise share one path, and the first to finish deleted the
/// input the second was still indexing.
fn incoming_pack_path(pack_dir: &std::path::Path) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    pack_dir.join(format!("incoming-{}-{seq}.pack", std::process::id()))
}

/// Sync half of `write_pack_of_objects`: the temp pack, the index, the cleanup.
fn index_objects(
    repo: &Repo,
    objects: &[(gix_object::Kind, Vec<u8>)],
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    std::fs::create_dir_all(&repo.pack_dir)?;
    let pack_path = incoming_pack_path(&repo.pack_dir);
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
    );
    let _ = std::fs::remove_file(&pack_path);
    let outcome = outcome?;
    if let Some(k) = outcome.keep_path {
        let _ = std::fs::remove_file(k);
    }
    match (outcome.data_path, outcome.index_path) {
        (Some(data), Some(index)) => Ok((data, index)),
        _ => Err(err("the new objects produced no pack")),
    }
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
    odb: &impl gix_object::FindExt,
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
                //
                // Read from the BASE TREE, not from the editor: `Editor::get` only
                // sees trees it has already loaded, so for `src/main.rs` it answers
                // None before anything has touched `src` -- and every edit to a
                // nested file silently came back as non-executable.
                let kind = match existing_kind(odb, base, &parts) {
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

/// The kind of the entry already at `parts`, read from `base`.
///
/// Walks the trees rather than asking the editor: the editor knows only what it
/// has loaded, which for an untouched path is nothing.
fn existing_kind(
    odb: &impl gix_object::FindExt,
    base: Option<ObjectId>,
    parts: &[&str],
) -> Option<gix_object::tree::EntryKind> {
    let mut cur = (base?, gix_object::tree::EntryKind::Tree);
    for seg in parts {
        if cur.1 != gix_object::tree::EntryKind::Tree {
            return None;
        }
        let mut buf = Vec::new();
        let t = gix_object::FindExt::find_tree(odb, &cur.0, &mut buf).ok()?;
        let e = t.entries.iter().find(|e| e.filename == seg.as_bytes())?;
        cur = (e.oid.to_owned(), e.mode.kind());
    }
    Some(cur.1)
}

/// A path's components, refused unless every one of them is a name git will
/// store and a client will check out.
///
/// `..` is the one that matters: a tree entry is a NAME, so a component that
/// means "the parent" cannot be stored — but a client checking the tree out
/// resolves it against the filesystem, which is a write outside the worktree.
// Git itself refuses these on checkout because NTFS/HFS silently normalize
// them away, so a tree that looks safe here can still land as `.git` on the
// filesystem: trailing dots/spaces (`.git.`, `.git `), the 8.3 short name
// (`git~1`), and HFS-ignorable codepoints woven into `.git` (`.g\u{200D}it`).
// `is_dotgit_variant` mirrors git's own `verify_dotfile`/`is_ntfs_dotgit`/
// `is_hfs_dotgit` checks closely enough to close the same hole.
fn is_dotgit_variant(p: &str) -> bool {
    let trimmed = p.trim_end_matches(['.', ' ']);
    if trimmed.eq_ignore_ascii_case(".git") {
        return true;
    }
    // 8.3 short name: any case of "git~" followed by digits (git~1, git~2, ...).
    if trimmed.len() > 4 && trimmed.as_bytes()[..4].eq_ignore_ascii_case(b"git~") {
        let digits = &trimmed[4..];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    // HFS treats these codepoints as invisible, so ".g\u{200D}it" reads as
    // ".git" on disk. Strip the ones git's fsck/checkout guard against.
    const HFS_IGNORABLE: [char; 5] = ['\u{200c}', '\u{200d}', '\u{2060}', '\u{feff}', '\u{206a}'];
    if p.chars().any(|c| HFS_IGNORABLE.contains(&c)) {
        let stripped: String = p.chars().filter(|c| !HFS_IGNORABLE.contains(c)).collect();
        if stripped.trim_end_matches(['.', ' ']).eq_ignore_ascii_case(".git") {
            return true;
        }
    }
    false
}

fn split_path(path: &str) -> Result<Vec<&str>> {
    if path.len() > 4096 {
        return Err(err("path is too long"));
    }
    let parts: Vec<&str> = path.split('/').collect();
    for p in &parts {
        let bad = p.is_empty()
            || *p == "."
            || *p == ".."
            || is_dotgit_variant(p)
            || p.contains('\\')
            || p.bytes().any(|b| b < 0x20 || b == 0x7f);
        if bad {
            return Err(err(format!("{path} is not a valid path")));
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod dotgit_variant_tests {
    use super::split_path;

    fn rejects(path: &str) {
        assert!(split_path(path).is_err(), "expected {path:?} to be rejected");
    }

    fn allows(path: &str) {
        assert!(split_path(path).is_ok(), "expected {path:?} to be allowed");
    }

    #[test]
    fn rejects_plain_dotgit_case_insensitive() {
        rejects(".git");
        rejects(".GIT");
        rejects("a/.Git/b");
    }

    #[test]
    fn rejects_ntfs_trailing_dot_or_space_variants() {
        rejects(".git.");
        rejects(".git ");
        rejects(".git...");
        rejects(".git   ");
        rejects(".GIT.");
    }

    #[test]
    fn rejects_short_name_variant() {
        rejects("git~1");
        rejects("GIT~1");
        rejects("git~42");
    }

    #[test]
    fn rejects_hfs_ignorable_codepoint_variant() {
        rejects(".g\u{200d}it");
        rejects(".g\u{200c}it");
        rejects(".g\u{feff}it");
    }

    #[test]
    fn allows_legitimate_names_containing_git() {
        allows("git.txt");
        allows("gitconfig");
        allows("src/git-helpers.rs");
        allows("git~notanumber");
        allows("legit");
    }
}

#[cfg(test)]
mod temp_name_tests {
    #[test]
    fn two_writers_of_the_same_content_get_different_temp_paths() {
        let dir = std::path::Path::new("/pack");
        let a = super::incoming_pack_path(dir);
        let b = super::incoming_pack_path(dir);
        assert_ne!(a, b, "a content-named temp let two identical merges delete each other's input");
        assert!(a.starts_with(dir) && a.extension().and_then(|x| x.to_str()) == Some("pack"));
    }
}

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

    // Written beside the repo's other packs, under a name that carries the object
    // id: two writers racing on the same commit produce the same file rather than
    // corrupting each other's.
    std::fs::create_dir_all(&repo.pack_dir)?;
    let pack_path = repo.pack_dir.join(format!("incoming-{}.pack", oid.to_hex()));
    write_one_object_pack(gix_object::Kind::Commit, &body, &pack_path)?;

    // Indexed through the push path, so a malformed object fails here rather than
    // becoming a ref nobody can read.
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
        return Err(err("the new commit produced no pack"));
    };
    store.upload_pack_files(repo, &data, &index).await?;
    Ok(oid)
}

/// A pack holding exactly one object, written by hand.
///
/// `gix-pack`'s writer builds from an odb the objects are already in, which is
/// the wrong way round here — the object does not exist yet. A single-object pack
/// is a 12-byte header, one zlib-compressed entry and a trailing checksum, so it
/// is written directly rather than by putting the object somewhere first just to
/// read it back.
fn write_one_object_pack(
    kind: gix_object::Kind,
    body: &[u8],
    path: &std::path::Path,
) -> Result<()> {
    use std::io::Write;
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes()); // version
    out.extend_from_slice(&1u32.to_be_bytes()); // one object

    // Entry header: type in bits 4-6, size in a base-128 varint whose FIRST group
    // is only 4 bits wide — the quirk that makes this format easy to get wrong.
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

    // The trailer is the SHA-1 of everything before it.
    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&out);
    let checksum = hasher.try_finalize().map_err(|e| err(e.to_string()))?;
    out.extend_from_slice(checksum.as_bytes());

    std::fs::write(path, &out)?;
    Ok(())
}

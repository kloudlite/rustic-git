use super::walk::{counts_with_leaves, Range};
use gix_hash::ObjectId;
use gix_pack::data::output::count::objects::ObjectExpansion;
use crate::{err, Result};
use std::io::Write;
use std::sync::atomic::AtomicBool;

/// Stream a pack for an EXPLICIT set of commits — a shallow fetch, where the walk
/// has already been done and stopped at the boundary.
///
/// Separate from `write_pack` rather than a flag on it, because the difference is
/// not a parameter: one decides which commits to send by walking, and walking is
/// exactly what a boundary forbids.
pub(super) fn write_pack_of(
    odb: &gix_odb::Handle,
    commits: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    // The client already has everything reachable from `haves`, and inside a
    // boundary "reachable" cannot run away into withheld history.
    let have: std::collections::HashSet<ObjectId> = haves.into_iter().collect();
    let ids: Vec<ObjectId> = commits.into_iter().filter(|c| !have.contains(c)).collect();
    // A shallow boundary's parent is withheld, so a diff against it would be a delta onto an
    // object the client never gets.
    pack_from_ids(odb, ids, ObjectExpansion::TreeContents, out, interrupt)
}

/// Stream a pack containing everything reachable from `wants` and not from `haves`.
pub(crate) fn write_pack(
    odb: &gix_odb::Handle,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    let range = super::walk::commit_range(odb, wants, haves)?;
    write_pack_range(odb, range, out, interrupt)
}

/// The pack for an already-computed [`Range`] — `fetch` computes the range once and shares it
/// with the include-tag decision; `write_pack` wraps the two for callers with plain wants.
pub(super) fn write_pack_range(
    odb: &gix_odb::Handle,
    range: Range,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    // pack entries are copied straight out of mapped packs, which must not be unloaded meanwhile
    let mut odb = odb.clone();
    odb.prevent_pack_unload();
    let odb = &odb;
    // Commits carry only what they ADD over their parents: the client either has the parent
    // (it was a `have`) or is getting it in this same pack. Expanding every commit's whole tree
    // instead made an incremental fetch cost O(repo) — each `git fetch` re-sent every blob.
    // A tree or blob wanted by id (a promisor fetch) is still expanded whole, as git does; its
    // pass is deduped against the first because each count has its own `seen` set.
    // ponytail: gix-pack's `TreeAdditionsComparedToAncestor` is wrong for a merge (upstream: GitoxideLabs/gitoxide#2935) — it clears the
    // change delegate inside the per-parent loop and only reads it after, so only the LAST
    // parent's additions survive; worse, `AllNew::visit` marks every addition seen as it records
    // it, so an addition found via an earlier parent is neither emitted here nor re-emitted when
    // another commit diffs and finds the same blob. So merges get their whole tree instead of
    // their additions — merges are a minority of commits, and the traversal in `commit_range`
    // already knows which ones, so this costs no re-decode. Drop this when gix-pack is fixed.
    let Range { ids, mut leaves, merges, .. } = range;
    leaves.extend(merges);
    let counts = counts_with_leaves(
        odb,
        ids,
        ObjectExpansion::TreeAdditionsComparedToAncestor,
        leaves,
        interrupt,
    )?;
    write_counts(odb, counts, out, interrupt)
}

/// Expand `ids` into the entries a pack will carry. One call has one `seen` set, so a caller
/// combining two passes has to dedup by id itself — a repeated entry is a corrupt pack.
pub(crate) fn count_objects(
    odb: &gix_odb::Handle,
    ids: Vec<ObjectId>,
    expansion: ObjectExpansion,
    interrupt: &AtomicBool,
) -> Result<Vec<gix_pack::data::output::Count>> {
    use gix_pack::data::output;
    let mut odb = odb.clone();
    odb.prevent_pack_unload();
    let (counts, _) = output::count::objects_unthreaded(
        &odb,
        &mut ids.into_iter().map(Ok),
        &gix_features::progress::Discard,
        interrupt,
        expansion,
    )?;
    Ok(counts)
}

/// Expand `ids` under `expansion` and stream them as a pack.
pub(super) fn pack_from_ids(
    odb: &gix_odb::Handle,
    ids: Vec<ObjectId>,
    expansion: ObjectExpansion,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    write_counts(
        odb,
        count_objects(odb, ids, expansion, interrupt)?,
        out,
        interrupt,
    )
}

/// Stream `counts` as a v2 pack.
pub(super) fn write_counts(
    odb: &gix_odb::Handle,
    counts: Vec<gix_pack::data::output::Count>,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    use gix_pack::data::output;
    let mut odb = odb.clone();
    odb.prevent_pack_unload();

    let num = counts.len() as u32;
    // ponytail: PackCopyAndBaseObjects reuses existing deltas but computes no new ones; fine until clones are measurably fat
    let entries = output::entry::iter_from_counts(
        counts,
        odb.clone(),
        Box::new(gix_features::progress::Discard),
        output::entry::iter_from_counts::Options {
            thread_limit: Some(1),
            mode: output::entry::iter_from_counts::Mode::PackCopyAndBaseObjects,
            allow_thin_pack: false,
            chunk_size: 1000,
            version: gix_pack::data::Version::V2,
            ..Default::default()
        },
    );
    let mut writer = output::bytes::FromEntriesIter::new(
        entries.map(|r| r.map(|(_, entries)| entries)),
        out,
        num,
        gix_pack::data::Version::V2,
        gix_hash::Kind::Sha1,
    );
    for r in &mut writer {
        if interrupt.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(err("client went away"));
        }
        r?;
    }
    Ok(())
}

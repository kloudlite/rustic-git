mod pack;
mod refs;
mod walk;

use super::{block_on, AGENT};
use crate::pktline::{self, BandWriter, Pkt};
use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use gix_pack::data::output::count::objects::ObjectExpansion;
use refs::{head_target, ls_refs, peel_to_object};
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;
use walk::{commit_range, counts_with_leaves, filtered_objects, ours, Deepen, Filter, Peeled};

pub(crate) use pack::write_pack;

pub fn advertise(out: &mut dyn Write) -> Result<()> {
    pktline::write_text(out, "version 2")?;
    pktline::write_text(out, AGENT)?;
    pktline::write_text(out, "ls-refs=unborn symrefs peel")?;
    pktline::write_text(out, "fetch=shallow filter wait-for-done ref-in-want")?;
    pktline::write_text(out, "object-format=sha1")?;
    pktline::write_flush(out)?;
    Ok(())
}

pub fn serve(
    store: &Store,
    repo: &Repo,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    loop {
        let cmd = match pktline::read_pkt(input)? {
            Some(Pkt::Data(d)) => String::from_utf8_lossy(&d).trim_end().to_string(),
            Some(Pkt::Flush) => continue,
            None | Some(_) => return Ok(()),
        };
        let cmd = cmd
            .strip_prefix("command=")
            .ok_or_else(|| err("expected command="))?
            .to_string();
        // capability lines (agent=..., object-format=...) until delim, then the argument list
        let mut args = Vec::new();
        loop {
            match pktline::read_pkt(input)? {
                Some(Pkt::Delim) => {
                    args = read_args(input)?;
                    break;
                }
                Some(Pkt::Data(_)) => {}
                _ => break,
            }
        }
        match cmd.as_str() {
            "ls-refs" => ls_refs(store, repo, &args, out)?,
            "fetch" => fetch(store, repo, &args, out, interrupt)?,
            _ => {
                pktline::write_text(out, &format!("ERR unknown command {cmd}"))?;
                pktline::write_flush(out)?;
                return Ok(());
            }
        }
    }
}

fn read_args(input: &mut dyn BufRead) -> Result<Vec<String>> {
    Ok(pktline::read_lines_until_flush(input)?
        .into_iter()
        .map(|l| String::from_utf8_lossy(&l).to_string())
        .collect())
}

fn fetch(
    store: &Store,
    repo: &Repo,
    args: &[String],
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut done = false;
    let mut wait_for_done = false;
    let mut deepen = Deepen::default();
    // `want-ref <name>`: the client names a REF instead of an oid, so it does not
    // have to run ls-refs first — and cannot race a ref that moves in between.
    let mut want_refs: Vec<String> = Vec::new();
    let mut include_tag = false;
    let mut filter: Option<Filter> = None;
    for a in args {
        if let Some(h) = a.strip_prefix("want ") {
            wants.push(ObjectId::from_hex(h.as_bytes()).map_err(|e| err(e.to_string()))?);
        } else if let Some(h) = a.strip_prefix("have ") {
            haves.push(ObjectId::from_hex(h.as_bytes()).map_err(|e| err(e.to_string()))?);
        } else if a == "done" {
            done = true;
        } else if a == "wait-for-done" {
            wait_for_done = true;
        } else if let Some(r) = a.strip_prefix("want-ref ") {
            want_refs.push(r.trim().to_string());
        } else if a == "include-tag" {
            include_tag = true;
        } else if let Some(n) = a.strip_prefix("deepen ") {
            deepen.depth = Some(n.trim().parse::<usize>().map_err(|_| err("bad deepen"))?.max(1));
        } else if a == "deepen-relative" {
            deepen.relative = true;
        } else if let Some(t) = a.strip_prefix("deepen-since ") {
            deepen.since = Some(t.trim().parse::<i64>().map_err(|_| err("bad deepen-since"))?);
        } else if let Some(r) = a.strip_prefix("deepen-not ") {
            // A ref name or an oid. Resolving here means the client does not have
            // to look it up first.
            let r = r.trim();
            match ObjectId::from_hex(r.as_bytes()) {
                Ok(o) => deepen.not.push(o),
                Err(_) => match block_on(store.list_refs(repo))?
                    .into_iter()
                    .find(|(name, _)| name == r || name.ends_with(&format!("/{r}")))
                {
                    Some((_, o)) => deepen.not.push(o),
                    // Refusing beats ignoring. A cutoff we cannot resolve would
                    // otherwise turn a request for a small clone into a silent
                    // full transfer — the client asked for less and would be
                    // billed for everything, with nothing saying so.
                    None => {
                        pktline::write_text(out, &format!("ERR deepen-not: no such ref {r}"))?;
                        return Ok(());
                    }
                },
            }
        } else if let Some(h) = a.strip_prefix("shallow ") {
            deepen
                .client_shallow
                .push(ObjectId::from_hex(h.trim().as_bytes()).map_err(|e| err(e.to_string()))?);
        } else if let Some(spec) = a.strip_prefix("filter ") {
            match Filter::parse(spec) {
                Some(f) => filter = Some(f),
                // Refused, not ignored. A filter we quietly drop turns a request
                // for a small clone into a full transfer with nothing to show
                // for it — the exact behaviour this feature exists to end.
                None => {
                    pktline::write_text(out, &format!("ERR filter {spec} not supported"))?;
                    return Ok(());
                }
            }
        }
        // no-progress, thin-pack, ofs-delta, include-tag, sideband-all: accepted/ignored
    }
    let odb = repo.odb()?;
    let all_refs = block_on(store.list_refs(repo))?;
    let tips: Vec<ObjectId> = all_refs.iter().map(|(_, o)| *o).collect();

    // Resolved before anything else uses `wants`, so a want-ref is indistinguishable
    // from an oid want from here on.
    let mut wanted: Vec<(String, ObjectId)> = Vec::new();
    for name in &want_refs {
        // HEAD is not stored; it is a rule about the other refs.
        let name = if name == "HEAD" { head_target(&all_refs) } else { name.clone() };
        match all_refs.iter().find(|(n, _)| **n == name) {
            Some((_, oid)) => {
                // Answered under the name the CLIENT asked for, which is what it
                // is waiting to hear back.
                wanted.push((name.clone(), *oid));
                wants.push(*oid);
            }
            // Named a ref we do not have: say so rather than quietly sending
            // nothing, which would look like an empty repo.
            None => {
                pktline::write_text(out, &format!("ERR upload-pack: not our ref {name}"))?;
                return Ok(());
            }
        }
    }
    // A `have` counts as common only if it is reachable from THIS repo's refs. Testing raw
    // existence in the shared network odb would answer "does any repo in this fork network have
    // object X?" — an existence oracle for a sibling repo's objects.
    let mut have_set: Option<std::collections::HashSet<ObjectId>> = None;
    let common: Vec<ObjectId> = if haves.is_empty() {
        Vec::new()
    } else {
        let ours_set = ours(&mut have_set, &odb, &tips)?;
        haves.iter().copied().filter(|h| ours_set.contains(h)).collect()
    };

    if !done {
        pktline::write_text(out, "acknowledgments")?;
        if common.is_empty() {
            pktline::write_text(out, "NAK")?;
            pktline::write_flush(out)?;
            return Ok(());
        }
        for c in &common {
            pktline::write_text(out, &format!("ACK {}", c.to_hex()))?;
        }
        if wait_for_done {
            // client asked us to keep negotiating until it says `done`
            pktline::write_flush(out)?;
            return Ok(());
        }
        pktline::write_text(out, "ready")?;
        pktline::write_delim(out)?;
    }
    // A ref tip is always fair game. Anything else has to be REACHABLE from this
    // repo's refs — which is what a partial clone's follow-up fetch asks for, when
    // it comes back for a blob it left behind.
    //
    // Reachable, not merely present: an object that a force-push orphaned is still
    // in the pack files, and answering for it would let anyone who learned an id
    // read content the branch no longer has. The same test already guards `have`.
    let tip_set: std::collections::HashSet<&ObjectId> = tips.iter().collect();
    let unknown: Vec<ObjectId> = wants.iter().copied().filter(|w| !tip_set.contains(w)).collect();
    if !unknown.is_empty() {
        let ours_set = ours(&mut have_set, &odb, &tips)?;
        if let Some(w) = unknown.iter().find(|w| !ours_set.contains(*w)) {
            pktline::write_text(out, &format!("ERR upload-pack: not our ref {}", w.to_hex()))?;
            return Ok(());
        }
    }

    // A shallow fetch is decided here, after the wants are known to be ours, and
    // reported BEFORE the pack: the client has to know where its history is cut
    // before it starts reading objects that stop there.
    let shallow = if deepen.asked() || !deepen.client_shallow.is_empty() {
        Some(walk::shallow_walk(&odb, &wants, &deepen)?)
    } else {
        None
    };
    if let Some(s) = &shallow {
        if !s.boundary.is_empty() || !s.unshallow.is_empty() {
            pktline::write_text(out, "shallow-info")?;
            for c in &s.boundary {
                pktline::write_text(out, &format!("shallow {}", c.to_hex()))?;
            }
            for c in &s.unshallow {
                pktline::write_text(out, &format!("unshallow {}", c.to_hex()))?;
            }
            pktline::write_delim(out)?;
        }
    }

    if !wanted.is_empty() {
        pktline::write_text(out, "wanted-refs")?;
        for (name, oid) in &wanted {
            pktline::write_text(out, &format!("{} {name}", oid.to_hex()))?;
        }
        pktline::write_delim(out)?;
    }

    // `include-tag`: carry any tag whose target is in the pack. This is why a
    // plain `git clone` normally arrives with tags — without it they are simply
    // absent, and nothing tells the person why.
    // Decided from the commits being sent, not a second walk of every object: a tag names a
    // commit in practice, and `commit_range` is O(commits) where the object walk is O(repo).
    // ponytail: a tag pointing straight at a tree or blob is not carried by include-tag; the
    // client fetches it by name on the next `git fetch --tags`.
    // The traversal is the expensive half of a fetch, and with include-tag it used to run
    // twice — once to decide which tags ride along, once to build the pack. A shallow fetch
    // already has its commit list (the shallow walk decided it), so it computes no range.
    let range = match &shallow {
        None => Some(commit_range(&odb, wants.clone(), common.clone())?),
        Some(_) => None,
    };

    let mut extra_tags: Vec<ObjectId> = Vec::new();
    if include_tag {
        let sending: std::collections::HashSet<ObjectId> = match (&shallow, &range) {
            (Some(s), _) => s.commits.iter().copied().collect(),
            (None, Some(r)) => r.ids.iter().copied().collect(),
            (None, None) => unreachable!("range exists whenever the fetch is not shallow"),
        };
        for (name, oid) in &all_refs {
            if !name.starts_with("refs/tags/") || wants.contains(oid) {
                continue;
            }
            let target = peel_to_object(&odb, *oid).unwrap_or(*oid);
            if sending.contains(&target) {
                extra_tags.push(*oid);
            }
        }
    }

    pktline::write_text(out, "packfile")?;
    let mut band = BandWriter { w: out, band: 1 };
    // With a boundary, the commit list is already decided — walking from the wants
    // again would run straight past it into the history being withheld.
    // The tags go in whichever way the pack is being built — a shallow fetch sends
    // an explicit object list, so appending to `wants` alone would drop them.
    let res = match (&shallow, filter, range) {
        // A filtered pack is an explicit object list by construction — every object in it was
        // chosen one by one — so it goes out AS IS, and both shallow and full go through the
        // same path once the commits are known. Expanding it would put back exactly the blobs
        // the filter removed.
        (shallow, Some(f), range) => {
            // A tree or blob wanted by id is a promisor fetch for that object: the filter does
            // not apply to it and it still expands whole, as git does.
            let (commits, leaves) = match (shallow, range) {
                (Some(s), _) => (s.commits.clone(), Vec::new()),
                (None, Some(r)) => (r.ids, r.leaves),
                (None, None) => unreachable!("range exists whenever the fetch is not shallow"),
            };
            let mut ids = filtered_objects(&odb, &commits, f)?;
            ids.extend(extra_tags);
            let have: std::collections::HashSet<ObjectId> = common.into_iter().collect();
            ids.retain(|id| !have.contains(id));
            counts_with_leaves(&odb, ids, ObjectExpansion::AsIs, leaves, interrupt)
                .and_then(|c| pack::write_counts(&odb, c, &mut band, interrupt))
        }
        (Some(s), None, _) => {
            let mut ids = s.commits.clone();
            ids.extend(extra_tags);
            pack::write_pack_of(&odb, ids, common, &mut band, interrupt)
        }
        (None, None, Some(mut r)) => {
            r.ids.extend(extra_tags);
            pack::write_pack_range(&odb, r, &mut band, interrupt)
        }
        (None, None, None) => unreachable!("range exists whenever the fetch is not shallow"),
    };
    if let Err(e) = res {
        // past the packfile header the only way to report failure is the error band
        let msg = e.to_string().replace('\n', " ");
        pktline::write_band(out, 3, format!("ERR {msg}\n").as_bytes())?;
    }
    pktline::write_flush(out)?;
    Ok(())
}

/// Every object reachable from `tips` (commits, their trees and blobs, peeled tags).
///
/// This is what "objects this repo legitimately has" means. It matters because a fork network
/// shares one object pool between repos: mere existence in the pool says nothing about whether
/// THIS repo may see the object.
///
/// ponytail: full enumeration per call, and `ours` above already memoizes it for the duration of
/// one fetch, so a fetch pays for it once however many times it asks. Nothing carries across
/// fetches — fine at repo sizes where a clone is fast; cache per (repo, tip-set) if it ever shows
/// up in latency.
pub(crate) fn reachable_set(
    odb: &gix_odb::Handle,
    tips: &[ObjectId],
) -> Result<std::collections::HashSet<ObjectId>> {
    reachable_set_hiding(odb, tips, &[], &AtomicBool::new(false))
}

/// Like [`reachable_set`], but stops the commit walk at `hide` (and its ancestors), so the result
/// is only what `tips` add on top of `hide`. Errors if any reachable object is missing from the
/// odb — which is exactly the "client sent a pack with holes" case.
pub(crate) fn reachable_set_hiding(
    odb: &gix_odb::Handle,
    tips: &[ObjectId],
    hide: &[ObjectId],
    interrupt: &AtomicBool,
) -> Result<std::collections::HashSet<ObjectId>> {
    use gix_pack::data::output;
    let mut odb = odb.clone();
    odb.prevent_pack_unload();
    let odb = &odb;

    // peel tags to commits so the walk has valid starting points; keep every id we touch
    let Peeled { commits, tags, leaves } = walk::peel_wants(odb, tips)?;
    let mut ids = tags;
    ids.extend(leaves);
    for info in gix_traverse::commit::Simple::new(commits, odb.clone()).hide(hide.iter().copied())? {
        ids.push(info?.id);
    }
    let (counts, _) = output::count::objects_unthreaded(
        odb,
        &mut ids.iter().copied().map(Ok),
        &gix_features::progress::Discard,
        interrupt,
        output::count::objects::ObjectExpansion::TreeContents,
    )?;
    let mut set: std::collections::HashSet<ObjectId> = ids.into_iter().collect();
    set.extend(counts.into_iter().map(|c| c.id));
    Ok(set)
}

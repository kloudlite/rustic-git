use super::{block_on, AGENT};
use crate::pktline::{self, BandWriter, Pkt};
use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;

pub fn advertise(out: &mut dyn Write) -> Result<()> {
    pktline::write_text(out, "version 2")?;
    pktline::write_text(out, AGENT)?;
    pktline::write_text(out, "ls-refs=unborn")?;
    // ponytail: no shallow/filter support advertised; those args are rejected with ERR below
    pktline::write_text(out, "fetch=wait-for-done")?;
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

fn ls_refs(store: &Store, repo: &Repo, args: &[String], out: &mut dyn Write) -> Result<()> {
    let symrefs = args.iter().any(|a| a == "symrefs");
    let unborn = args.iter().any(|a| a == "unborn");
    let prefixes: Vec<&str> = args
        .iter()
        .filter_map(|a| a.strip_prefix("ref-prefix "))
        .collect();
    let want = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let refs = block_on(store.list_refs(repo))?;
    let default = crate::refs::DEFAULT_BRANCH.to_string();
    // HEAD -> default branch if it exists, else master, else the first branch (like GitHub for
    // repos whose first push wasn't to the default branch); unborn default when there are none.
    let has = |b: &str| refs.iter().any(|(n, _)| n == &format!("refs/heads/{b}"));
    let head_target = if has(&default) || !refs.iter().any(|(n, _)| n.starts_with("refs/heads/")) {
        format!("refs/heads/{default}")
    } else if has("master") {
        "refs/heads/master".to_string()
    } else {
        refs.iter()
            .map(|(n, _)| n)
            .find(|n| n.starts_with("refs/heads/"))
            .cloned()
            .unwrap()
    };
    if want("HEAD") {
        match refs.iter().find(|(n, _)| *n == head_target) {
            Some((_, oid)) => pktline::write_text(
                out,
                &if symrefs {
                    format!("{} HEAD symref-target:{head_target}", oid.to_hex())
                } else {
                    format!("{} HEAD", oid.to_hex())
                },
            )?,
            None => {
                if unborn {
                    pktline::write_text(out, &format!("unborn HEAD symref-target:{head_target}"))?;
                }
            }
        }
    }
    for (name, oid) in &refs {
        if want(name) {
            pktline::write_text(out, &format!("{} {name}", oid.to_hex()))?;
        }
    }
    // ponytail: no 'peel' support (annotated tags not peeled in ls-refs); git works without it
    pktline::write_flush(out)?;
    Ok(())
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
    for a in args {
        if let Some(h) = a.strip_prefix("want ") {
            wants.push(ObjectId::from_hex(h.as_bytes()).map_err(|e| err(e.to_string()))?);
        } else if let Some(h) = a.strip_prefix("have ") {
            haves.push(ObjectId::from_hex(h.as_bytes()).map_err(|e| err(e.to_string()))?);
        } else if a == "done" {
            done = true;
        } else if a == "wait-for-done" {
            wait_for_done = true;
        } else if a.starts_with("deepen") || a.starts_with("shallow") || a.starts_with("filter") {
            pktline::write_text(
                out,
                &format!("ERR {} not supported", a.split(' ').next().unwrap_or("")),
            )?;
            return Ok(());
        }
        // no-progress, thin-pack, ofs-delta, include-tag, sideband-all: accepted/ignored
    }
    let odb = repo.odb()?;
    let tips: Vec<ObjectId> = block_on(store.list_refs(repo))?
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    // A `have` counts as common only if it is reachable from THIS repo's refs. Testing raw
    // existence in the shared network odb would answer "does any repo in this fork network have
    // object X?" — an existence oracle for a sibling repo's objects.
    let common: Vec<ObjectId> = if haves.is_empty() {
        Vec::new()
    } else {
        let ours = reachable_set(&odb, tips.clone())?;
        haves.iter().copied().filter(|h| ours.contains(h)).collect()
    };
    // ponytail: no ref-in-want, no include-tag; add if clients complain

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
    // like git's default (uploadpack.allowAnySHA1InWant=false): only ref tips of THIS repo may be
    // wanted — objects are shared across the fork network, so existence alone is not enough.
    for w in &wants {
        if !tips.contains(w) {
            pktline::write_text(out, &format!("ERR upload-pack: not our ref {}", w.to_hex()))?;
            return Ok(());
        }
    }

    pktline::write_text(out, "packfile")?;
    let mut band = BandWriter { w: out, band: 1 };
    let res = write_pack(&odb, wants, common, &mut band, interrupt);
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
/// ponytail: full enumeration each call, no cache — fine at repo sizes where a clone is fast;
/// memoize per (repo, tip-set) when it shows up in latency.
pub(crate) fn reachable_set(
    odb: &gix_odb::Handle,
    tips: Vec<ObjectId>,
) -> Result<std::collections::HashSet<ObjectId>> {
    reachable_set_hiding(odb, tips, Vec::new(), &AtomicBool::new(false))
}

/// Like [`reachable_set`], but stops the commit walk at `hide` (and its ancestors), so the result
/// is only what `tips` add on top of `hide`. Errors if any reachable object is missing from the
/// odb — which is exactly the "client sent a pack with holes" case.
pub(crate) fn reachable_set_hiding(
    odb: &gix_odb::Handle,
    tips: Vec<ObjectId>,
    hide: Vec<ObjectId>,
    interrupt: &AtomicBool,
) -> Result<std::collections::HashSet<ObjectId>> {
    use gix_pack::data::output;
    let mut odb = odb.clone();
    odb.prevent_pack_unload();
    let odb = &odb;

    // peel tags to commits so the walk has valid starting points; keep every id we touch
    let mut buf = Vec::new();
    let (mut commits, mut ids) = (Vec::new(), Vec::new());
    for t in tips {
        let mut id = t;
        loop {
            let obj = gix_object::FindExt::find(odb, &id, &mut buf)?;
            match obj.decode()? {
                gix_object::ObjectRef::Commit(_) => {
                    commits.push(id);
                    break;
                }
                gix_object::ObjectRef::Tag(tag) => {
                    ids.push(id);
                    id = tag.target();
                }
                _ => {
                    ids.push(id);
                    break;
                }
            }
        }
    }
    for info in gix_traverse::commit::Simple::new(commits, odb.clone()).hide(hide)? {
        ids.push(info?.id);
    }
    let (counts, _) = output::count::objects_unthreaded(
        odb,
        &mut ids.clone().into_iter().map(Ok),
        &gix_features::progress::Discard,
        interrupt,
        output::count::objects::ObjectExpansion::TreeContents,
    )?;
    let mut set: std::collections::HashSet<ObjectId> = ids.into_iter().collect();
    set.extend(counts.into_iter().map(|c| c.id));
    Ok(set)
}

/// Stream a pack containing everything reachable from `wants` and not from `haves`.
pub(crate) fn write_pack(
    odb: &gix_odb::Handle,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    use gix_pack::data::output;
    // pack entries are copied straight out of mapped packs, which must not be unloaded meanwhile
    let mut odb = odb.clone();
    odb.prevent_pack_unload();
    let odb = &odb;

    // Only commits can be walked. Tags are peeled to the commit they point at (the tag
    // objects themselves are sent as-is); trees and blobs are sent as-is too.
    let mut buf = Vec::new();
    let (mut tips, mut ids) = (Vec::new(), Vec::new());
    for w in &wants {
        let mut id = *w;
        loop {
            match gix_object::FindExt::find(odb, &id, &mut buf)?.decode()? {
                gix_object::ObjectRef::Commit(_) => {
                    tips.push(id);
                    break;
                }
                gix_object::ObjectRef::Tag(t) => {
                    ids.push(id);
                    id = t.target();
                }
                _ => {
                    ids.push(id);
                    break;
                }
            }
        }
    }
    let walk = gix_traverse::commit::Simple::new(tips, odb.clone()).hide(haves)?;
    for info in walk {
        ids.push(info?.id);
    }

    let (counts, _) = output::count::objects_unthreaded(
        odb,
        &mut ids.into_iter().map(Ok),
        &gix_features::progress::Discard,
        interrupt,
        output::count::objects::ObjectExpansion::TreeContents,
    )?;
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

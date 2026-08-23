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

/// Follow an annotated tag to the object it ultimately names.
///
/// `refs/tags/v1` may point at a tag object, which points at another tag, which
/// points at a commit. What a client wants is the commit at the end of that chain.
fn peel_to_object(odb: &gix_odb::Handle, mut id: ObjectId) -> Option<ObjectId> {
    let mut buf = Vec::new();
    let mut peeled = None;
    // Bounded: a tag chain is two or three long in practice, and a cycle in the
    // object database must not hang a ref listing.
    for _ in 0..16 {
        match gix_object::FindExt::find(odb, &id, &mut buf).ok()?.decode().ok()? {
            gix_object::ObjectRef::Tag(t) => {
                id = t.target();
                peeled = Some(id);
            }
            _ => return peeled,
        }
    }
    peeled
}

/// Which branch HEAD names.
///
/// The default branch if it exists, else master, else the first branch there is
/// (like GitHub, for repos whose first push was not to the default). Shared,
/// because `ls-refs` advertises HEAD and `want-ref HEAD` has to resolve to the
/// same thing — two copies of this rule is a clone that fetches a different
/// branch than it was shown.
fn head_target(refs: &[(String, ObjectId)]) -> String {
    let default = crate::refs::DEFAULT_BRANCH;
    let has = |b: &str| refs.iter().any(|(n, _)| n == &format!("refs/heads/{b}"));
    if has(default) || !refs.iter().any(|(n, _)| n.starts_with("refs/heads/")) {
        format!("refs/heads/{default}")
    } else if has("master") {
        "refs/heads/master".to_string()
    } else {
        refs.iter()
            .map(|(n, _)| n)
            .find(|n| n.starts_with("refs/heads/"))
            .cloned()
            .unwrap()
    }
}

fn ls_refs(store: &Store, repo: &Repo, args: &[String], out: &mut dyn Write) -> Result<()> {
    let symrefs = args.iter().any(|a| a == "symrefs");
    let peel = args.iter().any(|a| a == "peel");
    let unborn = args.iter().any(|a| a == "unborn");
    let prefixes: Vec<&str> = args
        .iter()
        .filter_map(|a| a.strip_prefix("ref-prefix "))
        .collect();
    let want = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let refs = block_on(store.list_refs(repo))?;
    let head_target = head_target(&refs);
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
    // Only opened when peeling was actually asked for: a listing should not pay
    // to open the object database to answer a question nobody put.
    let odb = if peel { repo.odb().ok() } else { None };
    for (name, oid) in &refs {
        if !want(name) {
            continue;
        }
        let peeled = odb
            .as_ref()
            .filter(|_| name.starts_with("refs/tags/"))
            .and_then(|o| peel_to_object(o, *oid));
        match peeled {
            Some(p) => pktline::write_text(out, &format!("{} {name} peeled:{}", oid.to_hex(), p.to_hex()))?,
            None => pktline::write_text(out, &format!("{} {name}", oid.to_hex()))?,
        }
    }
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
    // A ref tip is always fair game. Anything else has to be REACHABLE from this
    // repo's refs — which is what a partial clone's follow-up fetch asks for, when
    // it comes back for a blob it left behind.
    //
    // Reachable, not merely present: an object that a force-push orphaned is still
    // in the pack files, and answering for it would let anyone who learned an id
    // read content the branch no longer has. The same test already guards `have`.
    let unknown: Vec<ObjectId> = wants.iter().copied().filter(|w| !tips.contains(w)).collect();
    if !unknown.is_empty() {
        let ours = reachable_set(&odb, tips.clone())?;
        if let Some(w) = unknown.iter().find(|w| !ours.contains(*w)) {
            pktline::write_text(out, &format!("ERR upload-pack: not our ref {}", w.to_hex()))?;
            return Ok(());
        }
    }

    // A shallow fetch is decided here, after the wants are known to be ours, and
    // reported BEFORE the pack: the client has to know where its history is cut
    // before it starts reading objects that stop there.
    let shallow = if deepen.asked() || !deepen.client_shallow.is_empty() {
        Some(shallow_walk(&odb, &wants, &deepen)?)
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
    // ponytail: a second full enumeration to decide which tags are in range;
    // reuse the pack's own object set once write_pack reports it.
    let mut extra_tags: Vec<ObjectId> = Vec::new();
    if include_tag {
        let sending: std::collections::HashSet<ObjectId> = match &shallow {
            Some(s) => s.commits.iter().copied().collect(),
            None => reachable_set_hiding(&odb, wants.clone(), common.clone(), interrupt)?,
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
    let res = match (&shallow, filter) {
        // A filtered pack is an explicit object list by construction, so both
        // shallow and full go through the same path once the commits are known.
        (shallow, Some(f)) => {
            let commits = match shallow {
                Some(s) => s.commits.clone(),
                None => commit_range(&odb, wants.clone(), common.clone())?,
            };
            let mut ids = filtered_objects(&odb, &commits, f)?;
            ids.extend(extra_tags);
            write_pack_of(&odb, ids, common, &mut band, interrupt)
        }
        (Some(s), None) => {
            let mut ids = s.commits.clone();
            ids.extend(extra_tags);
            write_pack_of(&odb, ids, common, &mut band, interrupt)
        }
        (None, None) => {
            wants.extend(extra_tags);
            write_pack(&odb, wants, common, &mut band, interrupt)
        }
    };
    if let Err(e) = res {
        // past the packfile header the only way to report failure is the error band
        let msg = e.to_string().replace('\n', " ");
        pktline::write_band(out, 3, format!("ERR {msg}\n").as_bytes())?;
    }
    pktline::write_flush(out)?;
    Ok(())
}

/// What a client asked us to leave OUT of the pack — partial clone.
///
/// History stays whole; the bulk does not. The client records the server as a
/// "promisor" and comes back for individual objects when it actually needs them,
/// which is why `Fetch::wants` has to allow more than ref tips once this is on.
#[derive(Clone, Copy, PartialEq)]
enum Filter {
    /// No blobs at all — `blob:none`.
    NoBlobs,
    /// Blobs under this many bytes — `blob:limit=<n>`.
    BlobLimit(u64),
    /// Commits only, no trees and no blobs — `tree:0`.
    NoTrees,
}

impl Filter {
    fn parse(spec: &str) -> Option<Filter> {
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
fn parse_size(s: &str) -> Option<u64> {
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
fn filtered_objects(
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
        if let Ok(obj) = FindExt::find(odb, c, &mut buf) {
            if let Ok(gix_object::ObjectRef::Commit(commit)) = obj.decode() {
                trees.push(commit.tree());
            }
        }
    }

    while let Some(id) = trees.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Ok(tree) = odb.find_tree(&id, &mut buf) else { continue };
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
            } else if keep_blob(odb, child, filter) && seen.insert(child) {
                out.push(child);
            }
        }
    }
    Ok(out)
}

/// A blob's SIZE decides `blob:limit`, and the size is in the object header —
/// so this never inflates a blob to find out whether to send it.
fn keep_blob(odb: &gix_odb::Handle, id: ObjectId, filter: Filter) -> bool {
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
struct Deepen {
    /// `deepen <n>`: n commits back from each want. 1 means "just the tips".
    depth: Option<usize>,
    /// `deepen-since <unix>`: nothing committed before this.
    since: Option<i64>,
    /// `deepen-not <ref>`: stop when the walk reaches these.
    not: Vec<ObjectId>,
    /// `shallow <oid>`: boundaries the client already has. It re-sends these every
    /// time, which is why the server keeps no per-client state.
    client_shallow: Vec<ObjectId>,
    /// `deepen-relative`: depth counts from the client's existing boundary rather
    /// than from the tips.
    relative: bool,
}

impl Deepen {
    fn asked(&self) -> bool {
        self.depth.is_some() || self.since.is_some() || !self.not.is_empty()
    }
}

/// The commits a shallow fetch should send, and where its history is cut.
struct Shallow {
    /// Every commit inside the boundary — what the pack will carry.
    commits: Vec<ObjectId>,
    /// Commits whose parents are being withheld. The client records these as its
    /// new `.git/shallow`.
    boundary: Vec<ObjectId>,
    /// Commits the client had as a boundary that are now complete. This is what
    /// `--unshallow` reports.
    unshallow: Vec<ObjectId>,
}

/// Walk back from `wants`, stopping where the client asked.
///
/// Breadth-first by design: `depth` is measured in commits from the tip, so every
/// commit at distance n must be seen before any at n+1. A depth-first walk would
/// cut one long branch and leave a short one whole.
fn shallow_walk(odb: &gix_odb::Handle, wants: &[ObjectId], d: &Deepen) -> Result<Shallow> {
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
                gix_object::FindExt::find(odb, &p, &mut Vec::new())
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

/// The commits a fetch would send: reachable from `wants`, not from `haves`.
/// Split out so a filtered pack can decide what to do with each one.
fn commit_range(
    odb: &gix_odb::Handle,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let mut buf = Vec::new();
    let mut tips = Vec::new();
    let mut ids = Vec::new();
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
                // A want that is a tree or a blob is a promisor fetch: it is the
                // object itself, with nothing to walk.
                _ => {
                    ids.push(id);
                    break;
                }
            }
        }
    }
    for info in gix_traverse::commit::Simple::new(tips, odb.clone()).hide(haves)? {
        ids.push(info?.id);
    }
    Ok(ids)
}

/// Stream a pack for an EXPLICIT set of commits — a shallow fetch, where the walk
/// has already been done and stopped at the boundary.
///
/// Separate from `write_pack` rather than a flag on it, because the difference is
/// not a parameter: one decides which commits to send by walking, and walking is
/// exactly what a boundary forbids.
pub(crate) fn write_pack_of(
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
    pack_from_ids(odb, ids, out, interrupt)
}

/// Stream a pack containing everything reachable from `wants` and not from `haves`.
pub(crate) fn write_pack(
    odb: &gix_odb::Handle,
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
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
    pack_from_ids(odb, ids, out, interrupt)
}

/// Expand `ids` to everything they contain and stream it as a pack.
fn pack_from_ids(
    odb: &gix_odb::Handle,
    ids: Vec<ObjectId>,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    use gix_pack::data::output;
    let mut odb = odb.clone();
    odb.prevent_pack_unload();
    let odb = &odb;

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

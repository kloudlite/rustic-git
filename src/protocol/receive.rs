use super::{block_on, AGENT};
use crate::pktline::{self, BandWriter};
use crate::refs::RefUpdate;
use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;

const CAPS: &str = "report-status delete-refs side-band-64k ofs-delta";

pub fn advertise(store: &Store, repo: &Repo, out: &mut dyn Write) -> Result<()> {
    let refs = block_on(store.list_refs(repo))?;
    let caps = format!("{CAPS} {AGENT}");
    if refs.is_empty() {
        pktline::write_pkt(
            out,
            format!("{} capabilities^{{}}\0{caps}\n", "0".repeat(40)).as_bytes(),
        )?;
    } else {
        for (i, (name, oid)) in refs.iter().enumerate() {
            if i == 0 {
                pktline::write_pkt(out, format!("{} {name}\0{caps}\n", oid.to_hex()).as_bytes())?;
            } else {
                pktline::write_text(out, &format!("{} {name}", oid.to_hex()))?;
            }
        }
    }
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
    // 1. commands
    let mut updates = Vec::new();
    let mut client_caps = String::new();
    for line in pktline::read_lines_until_flush(input)? {
        let (cmd, caps) = match line.iter().position(|&b| b == 0) {
            Some(p) => (&line[..p], Some(&line[p + 1..])),
            None => (&line[..], None),
        };
        if let Some(c) = caps {
            client_caps = String::from_utf8_lossy(c).to_string();
        }
        let s = String::from_utf8_lossy(cmd).to_string();
        let mut parts = s.split(' ');
        let (old, new, name) = (
            parts.next().ok_or_else(|| err("bad cmd"))?,
            parts.next().ok_or_else(|| err("bad cmd"))?,
            parts.next().ok_or_else(|| err("bad cmd"))?,
        );
        let zero = "0".repeat(40);
        let parse = |h: &str| -> Result<Option<ObjectId>> {
            if h == zero {
                Ok(None)
            } else {
                Ok(Some(
                    ObjectId::from_hex(h.as_bytes()).map_err(|e| err(e.to_string()))?,
                ))
            }
        };
        if !valid_ref_name(name) {
            return Err(err("bad ref name"));
        }
        updates.push(RefUpdate {
            name: name.to_string(),
            old: parse(old)?,
            new: parse(new)?,
        });
    }
    if updates.is_empty() {
        return Ok(());
    }
    let sideband = client_caps.split(' ').any(|c| c == "side-band-64k");
    let report = client_caps.split(' ').any(|c| c == "report-status");

    // 2+3. index pack, upload, validate tips, apply refs.
    // Any failure here is reported to the client rather than aborting the stream.
    let mut results: Vec<Option<String>> = vec![None; updates.len()];
    let mut unpack_status = "ok".to_string();
    let mut fatal: Option<String> = None;
    if let Err(e) = apply(store, repo, input, &updates, &mut results, interrupt) {
        // A fence is not a per-ref failure to report and move on from: it means this node no
        // longer holds the repo, and the caller must re-route. Propagate it; the HTTP/SSH layer
        // turns it into a retry or a 503.
        if crate::pool::is_fenced(&e) {
            return Err(e);
        }
        let m = e.to_string().replace('\n', " ");
        unpack_status = format!("error {m}");
        fatal = Some(format!("unpack failed: {m}"));
        for r in results.iter_mut() {
            *r = Some(m.clone());
        }
    }

    // 4. report
    if let Some(m) = &fatal {
        if sideband {
            pktline::write_band(out, 3, m.as_bytes())?;
        }
    }
    if report {
        let mut body = Vec::new();
        pktline::write_text(&mut body, &format!("unpack {unpack_status}"))?;
        for (u, r) in updates.iter().zip(&results) {
            match r {
                None => pktline::write_text(&mut body, &format!("ok {}", u.name))?,
                Some(m) => pktline::write_text(&mut body, &format!("ng {} {}", u.name, m))?,
            }
        }
        pktline::write_flush(&mut body)?;
        if sideband {
            let mut bw = BandWriter { w: out, band: 1 };
            bw.write_all(&body)?;
            pktline::write_flush(out)?;
        } else {
            out.write_all(&body)?;
        }
    }
    Ok(())
}

/// The object ids contained in a freshly written pack index.
fn pack_object_ids(idx: &std::path::Path) -> Result<std::collections::HashSet<gix_hash::ObjectId>> {
    let file = gix_pack::index::File::at(idx, gix_hash::Kind::Sha1)?;
    Ok(file.iter().map(|e| e.oid).collect())
}

/// git's ref-name rules (git-check-ref-format), enough of them to keep hostile names out of
/// pkt-line output (control chars, newlines) and out of other repos via fork's ref copy.
pub(crate) fn valid_ref_name(name: &str) -> bool {
    if !name.starts_with("refs/")
        || name.len() > 512
        || name.ends_with('/')
        || name.ends_with(".lock")
    {
        return false;
    }
    if name.contains("..") || name.contains("//") || name.contains("@{") || name.contains("\\") {
        return false;
    }
    if name
        .split('/')
        .any(|c| c.is_empty() || c.starts_with('.') || c.ends_with(".lock"))
    {
        return false;
    }
    name.bytes()
        .all(|b| b > 0x20 && b != 0x7f && !b"~^:?*[".contains(&b))
}

/// Index+store the pack and apply the ref updates; fills `results` with per-ref rejections.
fn apply(
    store: &Store,
    repo: &Repo,
    input: &mut dyn BufRead,
    updates: &[RefUpdate],
    results: &mut [Option<String>],
    interrupt: &AtomicBool,
) -> Result<()> {
    // objects the client actually supplied in this push
    let mut pushed: std::collections::HashSet<gix_hash::ObjectId> = Default::default();
    // pack (only if some update creates/moves a ref)
    if updates.iter().any(|u| u.new.is_some()) {
        // input may have no more bytes if client sends only deletes; peek
        let has_data = input.fill_buf().map(|b| !b.is_empty()).unwrap_or(false);
        if has_data {
            if let Some((pack, idx)) = write_pack(repo, input, interrupt)? {
                // If the S3 upload fails, delete the just-indexed pack from the local cache before
                // returning. Otherwise this instance's odb would keep serving objects S3 lacks, and
                // the tip check below could accept a ref whose objects exist only here.
                if let Err(e) = block_on(store.upload_pack_files(repo, &pack, &idx)) {
                    let _ = std::fs::remove_file(&pack);
                    let _ = std::fs::remove_file(&idx);
                    return Err(e);
                }
                pushed = pack_object_ids(&idx)?;
            }
        }
    }

    // Full connectivity + isolation. For each new tip we walk its entire object closure, stopping
    // at the refs this repo already had (so the work is proportional to what the push adds), and
    // require every object in that closure to be either:
    //   * in the pack the client just sent, or
    //   * already reachable from this repo's own refs.
    // Two things fall out of this. A pack with holes fails the walk (the missing object can't be
    // read) instead of creating a ref whose history is broken. And because a fork network shares
    // one object pool, "exists in the odb" is NOT accepted — otherwise a client could point a ref
    // at a sibling repo's object, or push a commit referencing a sibling's tree, and clone it out.
    let odb = repo.odb()?;
    let old_tips: Vec<gix_hash::ObjectId> = block_on(store.list_refs(repo))?
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    let mut ours: Option<std::collections::HashSet<gix_hash::ObjectId>> = None;
    for (i, u) in updates.iter().enumerate() {
        let Some(n) = u.new else { continue };
        // objects this push adds on top of what the repo already had
        let added = match crate::protocol::upload::reachable_set_hiding(
            &odb,
            vec![n],
            old_tips.clone(),
            interrupt,
        ) {
            Ok(set) => set,
            // a missing object anywhere in the closure lands here
            Err(_) => {
                results[i] = Some("missing necessary objects".into());
                continue;
            }
        };
        let unexplained: Vec<&gix_hash::ObjectId> =
            added.iter().filter(|id| !pushed.contains(*id)).collect();
        if !unexplained.is_empty() {
            // Objects not in this push must already belong to this repo. Computing that set is the
            // expensive part, so it is done at most once per push and only when needed.
            // ponytail: full enumeration, no cache — memoize per (repo, tip-set) if pushes to large
            // repos get slow.
            let ours = match &ours {
                Some(set) => set,
                None => ours.insert(crate::protocol::upload::reachable_set(
                    &odb,
                    old_tips.clone(),
                )?),
            };
            if unexplained.iter().any(|id| !ours.contains(*id)) {
                results[i] = Some("missing necessary objects".into());
            }
        }
    }
    let owned: Vec<RefUpdate> = updates
        .iter()
        .enumerate()
        .filter(|(i, _)| results[*i].is_none())
        .map(|(_, u)| RefUpdate {
            name: u.name.clone(),
            old: u.old,
            new: u.new,
        })
        .collect();
    if owned.is_empty() {
        return Ok(());
    }
    let r = block_on(store.update_refs(repo, &owned))?;
    // update_refs is all-or-nothing: if any entry was rejected, nothing was applied.
    let atomic_fail = r.iter().any(|x| x.is_some());
    let mut j = 0;
    for res in results.iter_mut() {
        if res.is_none() {
            *res = match (&r[j], atomic_fail) {
                (Some(m), _) => Some(m.clone()),
                (None, true) => Some("atomic push failed".into()),
                (None, false) => None,
            };
            j += 1;
        }
    }
    Ok(())
}

/// Index the incoming pack into repo.pack_dir; returns (pack_path, idx_path), or None if pack was empty.
fn write_pack(
    repo: &Repo,
    input: &mut dyn BufRead,
    should_interrupt: &AtomicBool,
) -> Result<Option<(std::path::PathBuf, std::path::PathBuf)>> {
    let odb = repo.odb()?;
    let mut progress = gix_features::progress::Discard;
    let opts = gix_pack::bundle::write::Options {
        thread_limit: None,
        iteration_mode: gix_pack::data::input::Mode::Verify,
        index_version: gix_pack::index::Version::V2,
        object_hash: gix_hash::Kind::Sha1,
        alloc_limit_bytes: Some(1024 * 1024 * 1024), // 1 GiB per-object cap: reject zlib/delta bombs
        compression: Default::default(),
    };
    let outcome = gix_pack::Bundle::write_to_directory(
        input,
        Some(&repo.pack_dir),
        &mut progress,
        should_interrupt,
        Some(odb),
        opts,
    )?;
    if let Some(k) = outcome.keep_path {
        let _ = std::fs::remove_file(k);
    }
    match (outcome.data_path, outcome.index_path) {
        (Some(p), Some(i)) => Ok(Some((p, i))),
        _ => Ok(None),
    }
}

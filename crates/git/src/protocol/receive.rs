use super::{block_on, AGENT};
use crate::pktline::{self, BandWriter};
use crate::refs::RefUpdate;
use crate::store::{Repo, Store};
use crate::{err, Result};
use gix_hash::ObjectId;
use std::io::{BufRead, Write};
use std::sync::atomic::AtomicBool;

/// `atomic` is advertised because `update_refs` already IS all-or-nothing — every
/// push, whether or not the client asks. Not saying so left clients that need the
/// guarantee with no way to ask for it, and made the behaviour a surprise rather
/// than a contract.
const CAPS: &str =
    "report-status report-status-v2 delete-refs side-band-64k ofs-delta atomic push-options quiet";

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
        // Lossy decoding would silently swap invalid bytes for U+FFFD, so the ref name we
        // store could differ from the bytes the client actually sent. Reject the whole
        // command instead — same handling as any other malformed `old new name` line below.
        let s = std::str::from_utf8(cmd)
            .map_err(|_| err("bad ref name"))?
            .to_string();
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
        if !crate::refs::valid_ref_name(name) {
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
    let cap = |c: &str| client_caps.split(' ').any(|x| x == c);
    let sideband = cap("side-band-64k");
    // v2 is a superset, so a client asking for it is asking for a report.
    let report_v2 = cap("report-status-v2");
    let report = report_v2 || cap("report-status");

    // `git push -o key=value`. They arrive between the commands and the pack, so
    // they must be read even when nothing consumes them yet — leaving them in the
    // stream would make the pack parser read option text as pack bytes.
    let push_options: Vec<String> = if cap("push-options") {
        pktline::read_lines_until_flush(input)?
            .into_iter()
            .map(|l| String::from_utf8_lossy(&l).trim_end().to_string())
            .collect()
    } else {
        Vec::new()
    };
    if !push_options.is_empty() {
        // ponytail: accepted and recorded, consumed by nothing yet — CI Triggers
        // is the intended reader.
        //
        // Debug (`?`, not `%`) is load-bearing: Debug-formatting a str escapes control
        // bytes (ESC, CR, etc.) as `\u{..}`, so an attacker-controlled option value
        // can't inject ANSI/log-forging sequences into an operator's terminal.
        tracing::info!(push_options = ?push_options, "push options");
    }

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
                None => {
                    pktline::write_text(&mut body, &format!("ok {}", u.name))?;
                    // v2 lets the server say what the ref ended up as. Only sent
                    // when the client asked for v2: an `option` line to a v1
                    // client is a protocol error, not a nicety it ignores.
                    if report_v2 {
                        if let Some(new) = u.new {
                            pktline::write_text(
                                &mut body,
                                &format!("option new-oid {}", new.to_hex()),
                            )?;
                        }
                    }
                }
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

/// Whether a connectivity-walk error is "an object is not in the odb", as opposed to a read that
/// failed. Matched on the OUTER types `reachable_set_hiding` can return — every gix wrapper here is
/// What one push's connectivity check may take as given, carried across its refs.
#[derive(Default)]
struct Known {
    /// objects the client actually supplied in this push
    pushed: std::collections::HashSet<gix_hash::ObjectId>,
    /// Everything under the trees of the commits the new history grows from — what an unchanged
    /// subtree is explained by. Each such commit is expanded once, however many refs share it.
    boundary: std::collections::HashSet<gix_hash::ObjectId>,
    expanded: std::collections::HashSet<gix_hash::ObjectId>,
    /// Proven already: sent and everything under it checked, or ours.
    verified: std::collections::HashSet<gix_hash::ObjectId>,
    /// The whole-repo closure — the last resort, built at most once per push.
    ours: Option<std::collections::HashSet<gix_hash::ObjectId>>,
}

impl Known {
    /// Whether every object `range` needs is in the pack or already this repo's.
    ///
    /// Bounded by the push, not the repo: only a tree the client SENT is opened, an unchanged
    /// subtree is matched against the trees of the commits the new history grows from (what
    /// git's own check reads), and only what neither explains — a blob revived from older
    /// history, a ref pushed at an existing tag — pays for the whole-repo closure. Every new
    /// commit is checked, not just the tip: a hole in the middle of the range is still a hole.
    fn explains(
        &mut self,
        odb: &gix_odb::Handle,
        range: crate::protocol::upload::Range,
        old_tips: &[gix_hash::ObjectId],
        interrupt: &AtomicBool,
    ) -> Result<bool> {
        use crate::protocol::upload::{count_objects, reachable_set, walked};
        use gix_object::{FindExt, ObjectRef};
        use gix_pack::data::output::count::objects::ObjectExpansion;
        let crate::protocol::upload::Range { ids, leaves, boundary, .. } = range;
        let mut buf = Vec::new();
        for c in boundary {
            if !self.expanded.insert(c) {
                continue;
            }
            let tree = odb.find_commit(&c, &mut buf)?.tree();
            let counts = count_objects(odb, vec![tree], ObjectExpansion::TreeContents, interrupt)?;
            walked(counts.len());
            self.boundary.extend(counts.into_iter().map(|c| c.id));
        }
        let mut todo = ids;
        todo.extend(leaves);
        while let Some(id) = todo.pop() {
            if !self.verified.insert(id) {
                continue;
            }
            walked(1);
            if self.pushed.contains(&id) {
                // The client sent it, so it is here to read, and what it points at must be
                // explained too. A commit's parents are not followed: each is either in the
                // range (on `todo` already) or hidden, which is ours.
                match FindExt::find(odb, &id, &mut buf)?.decode()? {
                    ObjectRef::Commit(c) => todo.push(c.tree()),
                    ObjectRef::Tag(t) => todo.push(t.target()),
                    ObjectRef::Tree(t) => todo.extend(
                        t.entries.iter().filter(|e| !e.mode.is_commit()).map(|e| e.oid.to_owned()),
                    ),
                    ObjectRef::Blob(_) => {}
                }
            } else if !self.boundary.contains(&id) {
                // ponytail: full enumeration, no cache — memoize per (repo, tip-set) if pushes
                // that revive old blobs get slow.
                if self.ours.is_none() {
                    self.ours = Some(reachable_set(odb, old_tips)?);
                }
                if !self.ours.as_ref().expect("just filled").contains(&id) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

/// `#[error(transparent)]`, which forwards `source()` past itself, so the `NotFound` variant is
/// never a link in the chain and has to be read through each wrapper's own enum.
fn is_missing_object(e: &crate::Error) -> bool {
    use gix_object::find::{existing, existing_iter};
    use gix_pack::data::output::count::objects::Error as Count;
    use gix_traverse::{commit::simple::Error as Walk, tree::breadthfirst::Error as Tree};
    let one = |e: &existing::Error| matches!(e, existing::Error::NotFound { .. });
    let iter = |e: &existing_iter::Error| matches!(e, existing_iter::Error::NotFound { .. });
    // peel_wants: a tip that names nothing
    e.downcast_ref::<existing::Error>().is_some_and(one)
        // the commit walk: a parent that is not there
        || e.downcast_ref::<Walk>().is_some_and(|w| matches!(w, Walk::Find(f) if iter(f)))
        // tree expansion: a tree or blob a commit points at that is not there
        || e.downcast_ref::<Count>().is_some_and(|c| match c {
            Count::FindExisting(f) => one(f),
            Count::TreeTraverse(Tree::Find(f)) => iter(f),
            _ => false,
        })
}

#[cfg(test)]
mod missing_object_tests {
    use super::*;

    /// "Not in the odb" is the pusher's problem; a read that failed is ours and must propagate.
    #[test]
    fn only_a_not_found_is_the_pushers_fault() {
        let oid = gix_hash::ObjectId::null(gix_hash::Kind::Sha1);
        let missing: crate::Error = Box::new(gix_object::find::existing::Error::NotFound { oid });
        assert!(is_missing_object(&missing));
        let walk: crate::Error = Box::new(gix_traverse::commit::simple::Error::Find(
            gix_object::find::existing_iter::Error::NotFound { oid },
        ));
        assert!(is_missing_object(&walk));
        let io: crate::Error = Box::new(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "pack"));
        assert!(!is_missing_object(&io));
        assert!(!is_missing_object(&crate::err("store: timeout")));
    }
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
    // path of THIS push's freshly-written pack, if any — tracked so a fully-rejected push can
    // delete exactly what it added and nothing reachable from an existing ref.
    let mut this_push_pack: Option<(std::path::PathBuf, std::path::PathBuf)> = None;
    if updates.iter().any(|u| u.new.is_some()) {
        // input may have no more bytes if client sends only deletes; peek
        let has_data = input.fill_buf().map(|b| !b.is_empty()).unwrap_or(false);
        if has_data {
            if let Some((pack, idx)) = write_pack(repo, input, interrupt)? {
                pushed = pack_object_ids(&idx)?;
                this_push_pack = Some((pack, idx));
            }
        }
    }

    // Full connectivity + isolation. For each new tip we walk the commits it adds, stopping at
    // the refs this repo already had, and require every object those commits need to be either:
    //   * in the pack the client just sent, or
    //   * already reachable from this repo's own refs.
    // Two things fall out of this. A pack with holes fails the walk (the missing object is in no
    // set) instead of creating a ref whose history is broken. And "exists in the local odb" is
    // NOT accepted, because the cache can hold objects this repo does not own: a pack from a push
    // that was rejected after indexing, or a pack a repack elsewhere has since dropped and the
    // prune has not yet reached.
    let odb = repo.odb()?;
    let old_tips: Vec<gix_hash::ObjectId> = block_on(store.list_refs(repo))?
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    let mut known = Known { pushed, ..Default::default() };
    // Grows with every tip accepted so far, so a push of 20 branches off one base walks their
    // shared history once instead of 20 times. Hiding an ACCEPTED tip cannot hide a problem: its
    // own closure was just proven to be entirely in this pack or already ours, so anything a later
    // ref reaches through it is explained too. This is what git itself does.
    let mut hide = old_tips.clone();
    for (i, u) in updates.iter().enumerate() {
        let Some(n) = u.new else { continue };
        // the commits this push adds on top of what the repo already had
        let range = match crate::protocol::upload::range_over(&odb, vec![n], hide.clone()) {
            Ok(r) => r,
            // a tip, or a parent on the way down to our refs, that is not there
            Err(e) if is_missing_object(&e) => {
                results[i] = Some("missing necessary objects".into());
                continue;
            }
            // Anything else — an unreadable pack file, a corrupt object — is this node's fault,
            // and telling the pusher their pack has holes sends them debugging the wrong side.
            Err(e) => return Err(e),
        };
        if !known.explains(&odb, range, &old_tips, interrupt)? {
            results[i] = Some("missing necessary objects".into());
        }
        if results[i].is_none() {
            hide.push(n);
        }
    }
    // `atomic` is advertised, so one ref failing the walk fails the batch — applying the survivors
    // would be exactly the partial push the client asked not to have. The pack was never uploaded,
    // so there is nothing in the object store to undo — only the local files this node indexed.
    if results.iter().any(|r| r.is_some()) {
        for r in results.iter_mut() {
            if r.is_none() {
                *r = Some("atomic push failed".into());
            }
        }
        if let Some((pack, idx)) = &this_push_pack {
            let _ = std::fs::remove_file(pack);
            let _ = std::fs::remove_file(idx);
        }
        return Ok(());
    }
    // Uploaded only now that every update has survived connectivity, so a broken or hostile
    // client costs a local index instead of a full multipart upload plus a delete. Still strictly
    // BEFORE `update_refs`, which is what the ordering was always for: a ref must never be
    // published pointing at objects that exist only on this node.
    //
    // On failure, drop the just-indexed pack from the local cache too — otherwise this instance's
    // odb would keep serving objects S3 lacks.
    if let Some((pack, idx)) = &this_push_pack {
        if let Err(e) = block_on(store.upload_pack_files(repo, pack, idx)) {
            let _ = std::fs::remove_file(pack);
            let _ = std::fs::remove_file(idx);
            return Err(e);
        }
    }
    let r = block_on(crate::refs::update_refs(store, repo, updates))?;
    // update_refs is all-or-nothing: if any entry was rejected, nothing was applied.
    let atomic_fail = r.iter().any(|x| x.is_some());
    if atomic_fail {
        // Same reasoning as the connectivity branch above: nothing from this batch landed
        // (branch protection is one way a single entry can reject the whole atomic update), so
        // nothing reachable points at this push's pack.
        if let Some((pack, idx)) = &this_push_pack {
            let _ = block_on(store.delete_pack_files(repo, pack, idx));
        }
    }
    for (res, v) in results.iter_mut().zip(r) {
        *res = v.or_else(|| atomic_fail.then(|| "atomic push failed".into()));
    }
    Ok(())
}

/// A `BufRead` that errors once more than `left` bytes have gone through it.
///
/// HTTP enforces `max_body` in the extractor before a handler runs; SSH hands this module a raw
/// channel with nothing in front of it. The cap sits here, where both transports feed the pack
/// through, so an authenticated pusher cannot stream a pack until the node's disk is full. It
/// errors rather than truncating: a `Take` would hand the indexer a clean EOF and the pusher a
/// baffling "pack truncated" instead of the reason.
struct Capped<'a> {
    inner: &'a mut dyn BufRead,
    left: u64,
    hit_cap: bool,
}

impl std::io::Read for Capped<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = {
            let src = self.fill_buf()?;
            let n = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Capped<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        let b = self.inner.fill_buf()?;
        if self.left == 0 && !b.is_empty() {
            self.hit_cap = true;
            return Err(std::io::Error::other("pack exceeds the size limit"));
        }
        let n = (b.len() as u64).min(self.left) as usize;
        Ok(&b[..n])
    }
    fn consume(&mut self, n: usize) {
        // Saturating, not `-=`: a caller that consumes more than `fill_buf` handed back is
        // violating the `BufRead` contract, but wrapping here would silently lift the cap.
        self.left = self.left.saturating_sub(n as u64);
        self.inner.consume(n);
    }
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
    let mut capped =
        Capped { inner: input, left: rustic_git_core::httpx::max_body() as u64, hit_cap: false };
    let outcome = match gix_pack::Bundle::write_to_directory(
        &mut capped,
        Some(&repo.pack_dir),
        &mut progress,
        should_interrupt,
        Some(odb),
        opts,
    ) {
        Ok(o) => o,
        // gix buries the io error's message under its own ("a pack entry could not be
        // extracted"), which tells the pusher nothing. The reader records that it was the cap
        // that failed the read, so say what actually happened.
        Err(_) if capped.hit_cap => return Err(err("pack exceeds the size limit")),
        Err(e) => return Err(e.into()),
    };
    if let Some(k) = outcome.keep_path {
        let _ = std::fs::remove_file(k);
    }
    match (outcome.data_path, outcome.index_path) {
        (Some(p), Some(i)) => Ok(Some((p, i))),
        _ => Ok(None),
    }
}

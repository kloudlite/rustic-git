# Review Fixes — Git Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the git-core findings of the 2026-08-23 review — fetch bandwidth, the SSH pack cap, pool handle leaks, stale local packs, signature payloads, runtime-thread blocking — each behind its own test and commit.

**Architecture:** Every task is an independent fix in the file the finding names, following the sibling code already there. No new modules, no new dependencies. Tasks are ordered High → Medium → Low → cleanups; the only cross-task dependency is Task 22 (`peel_wants`) which assumes Task 1's `write_pack` shape, and Task 21 which waits on the registry plan's `crate::hex`.

**Tech Stack:** Rust, tokio, gix-pack 0.73 / gix-object 0.63 / gix-traverse 0.60, SlateDB 0.15, redis 0.27 (`script` feature is in its defaults), pgp 0.20.

**Spec:** `docs/code-review-2026-08-23.md` — sections 0 (#4, #5), 2 (Medium/Low git-core rows), 3, 4, 5, 6. The plan argues from it; read both.

## Global Constraints

- `cargo test` green after every task (run the named `--test` file while iterating, the full suite before each commit).
- Clippy bar from `CLAUDE.md`: no NEW warnings in files you touch; `--all-targets -D warnings` has ~15 pre-existing errors — ignore those.
- House style: comments explain WHY; keep every `// ponytail:` marker you edit near, and add one where a task deliberately leaves a ceiling. Commit subjects imperative sentence case, no tool attribution.
- Integration tests that shell out to git start with `if !common::have_git() { eprintln!("skip: no git"); return; }` and use `#[tokio::test(flavor = "multi_thread")]` — `protocol::block_on` uses `block_in_place`, which panics on a current-thread runtime.
- Env vars are process-global and tests in one file share a process: a test that sets one gets its own `tests/*.rs` file.
- Do not touch `src/http/browse_api/merge.rs` or `src/directory.rs` — another plan owns them. Do not change the blob-deletion, verbatim-manifest or `Digest::parse` rules.

**Findings deliberately NOT in this plan** (covered elsewhere or already done):
- "Pack with holes rejected" — `tests/protocol.rs::gappy_pack_is_rejected` already exists and passes.
- `directory.rs:379` `claim_username` race — other plan.
- `browse_api/merge.rs:116,286` — other plan; this plan only makes `objects.rs`/`refs.rs` blocking-safe (Tasks 9, 10).

---

## HIGH

### Task 1: Incremental fetch sends only what the client lacks

**Files:**
- Modify: `src/protocol/upload.rs:757-848` (`write_pack`, `pack_from_ids`)
- Test: `tests/protocol.rs`

**Context:** `pack_from_ids` counts objects with `ObjectExpansion::TreeContents`: every commit in the range (wants minus haves) is expanded to its WHOLE tree, so a fetch of one small commit re-sends every blob in the repo. `TreeAdditionsComparedToAncestor` (verified in `~/.cargo/registry/src/*/gix-pack-0.73.0/src/data/output/count/objects/mod.rs:167-272`) does per input commit: push the commit + its root tree, then for each parent push the parent commit + parent root tree and diff parent tree → this tree, pushing only added/modified trees and blobs; a root commit (no parents) gets a full tree traversal. Two consequences drive the design:

1. It is correct for a walked commit list: every object of commit C is either in C's diff against a parent, or in that parent's own expansion (recursively to a root, or to a `have` the client already holds). Boundary parents (haves) get their commit + root tree object pushed too — a few hundred bytes the client already has, harmless.
2. It is WRONG for the shallow path: a boundary commit's parent is withheld, so the diff-vs-parent would send a delta against a tree the client never gets. `write_pack_of` (shallow) keeps `TreeContents`. And for a wanted tree or blob (promisor fetch) it sends the object as-is with no contents, where `TreeContents` expands a tree — git expands, so keep `TreeContents` for those.

So `write_pack` counts commits (and tags, which peel to commits) with `TreeAdditionsComparedToAncestor`, trees/blobs wanted directly with `TreeContents`, and dedups the second pass against the first (each `objects_unthreaded` call has its own `seen` set; a duplicate entry in a pack is a corrupt pack).

**Interfaces:**
- Produces: `fn count_objects(odb, ids: Vec<ObjectId>, expansion: ObjectExpansion, interrupt) -> Result<Vec<gix_pack::data::output::Count>>`, `fn write_counts(odb, counts, out, interrupt) -> Result<()>`, and `pack_from_ids` gains an `expansion` parameter. Task 2 uses the parameter; Task 22 reshapes `write_pack`'s peel loop.

- [ ] **Step 1: Write the failing test**

Append to `tests/protocol.rs`:

```rust
/// Drive `upload::serve` with one fetch command and return the raw pack bytes it streamed.
fn fetch_pack_bytes(
    s: &std::sync::Arc<kloudlite::store::Store>,
    repo: &kloudlite::store::Repo,
    lines: &[String],
) -> Vec<u8> {
    let mut req = Vec::new();
    pktline::write_text(&mut req, "command=fetch").unwrap();
    pktline::write_text(&mut req, "object-format=sha1").unwrap();
    pktline::write_delim(&mut req).unwrap();
    pktline::write_text(&mut req, "no-progress").unwrap();
    for l in lines {
        pktline::write_text(&mut req, l).unwrap();
    }
    pktline::write_text(&mut req, "done").unwrap();
    pktline::write_flush(&mut req).unwrap();
    let mut out = Vec::new();
    upload::serve(s, repo, &mut Cursor::new(req), &mut out, &Default::default()).unwrap();
    let mut c = Cursor::new(out);
    let (mut pack, mut in_pack) = (Vec::new(), false);
    while let Some(p) = pktline::read_pkt(&mut c).unwrap() {
        if let pktline::Pkt::Data(d) = p {
            if in_pack {
                if d[0] == 1 {
                    pack.extend_from_slice(&d[1..]);
                }
            } else if d == b"packfile\n" {
                in_pack = true;
            }
        }
    }
    assert!(pack.starts_with(b"PACK"), "no pack came back");
    pack
}

/// An incremental fetch must cost O(what changed), not O(repo). With the tree snapshot
/// expansion every `git fetch` re-sent every blob; this pins the pack to the delta.
#[tokio::test(flavor = "multi_thread")]
async fn incremental_fetch_sends_the_delta_not_the_snapshot() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "big", |c| {
        // 200 incompressible 4 KiB files, so the pack size reflects the blobs carried.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for i in 0..200 {
            let mut body = Vec::with_capacity(4096);
            while body.len() < 4096 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                body.extend_from_slice(&x.to_le_bytes());
            }
            std::fs::write(c.join(format!("f{i}.bin")), &body).unwrap();
        }
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "snapshot"]);
        std::fs::write(c.join("f0.bin"), b"tiny change\n").unwrap();
        common::git(c, &["commit", "-qam", "one file"]);
    })
    .await;
    let s = e.store.clone();
    let head = s.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let odb = repo.odb().unwrap();
    let parent = gix_object::FindExt::find_commit(&odb, &head, &mut Vec::new())
        .unwrap()
        .parents()
        .next()
        .unwrap();

    let (s2, r2) = (s.clone(), repo.clone());
    let full = tokio::task::spawn_blocking(move || {
        fetch_pack_bytes(&s2, &r2, &[format!("want {head}")])
    })
    .await
    .unwrap();
    let (s2, r2) = (s.clone(), repo.clone());
    let incremental = tokio::task::spawn_blocking(move || {
        fetch_pack_bytes(&s2, &r2, &[format!("want {head}"), format!("have {parent}")])
    })
    .await
    .unwrap();

    assert!(full.len() > 700 * 1024, "fixture is big enough to measure: {}", full.len());
    assert!(
        incremental.len() * 20 < full.len(),
        "incremental {} bytes vs clone {} bytes: the snapshot was re-sent",
        incremental.len(),
        full.len()
    );

    // And the delta pack is complete: a client holding `parent` can index it and read HEAD.
    let scratch = tempfile::tempdir().unwrap();
    common::git(scratch.path(), &["init", "-q"]);
    for pack in [&full, &incremental] {
        let mut c = std::process::Command::new("git")
            .args(["index-pack", "--stdin"])
            .current_dir(scratch.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(pack).unwrap();
        let out = c.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    common::git(scratch.path(), &["cat-file", "-e", &head.to_hex().to_string()]);
    common::git(scratch.path(), &["fsck", "--no-progress", "--connectivity-only", &head.to_hex().to_string()]);
}
```

`gix_object` is not a dev-dependency name in `tests/` — use `gix_object::FindExt` via the crate graph (it is a direct dependency, so `tests/` can `use` it). If the compiler complains, replace the parent lookup with `common::git(&src, &["rev-parse", "HEAD~1"])` by keeping the work tree: `push_built` discards it, so instead read the parent from `browse::log(&odb, head, 2)[1].oid`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test protocol incremental_fetch_sends_the_delta_not_the_snapshot`
Expected: FAIL on the `incremental.len() * 20 < full.len()` assertion — the incremental pack is roughly the size of the clone.

- [ ] **Step 3: Split counting from writing and give `write_pack` two passes**

Replace `pack_from_ids` (upload.rs:800-848) with:

```rust
use gix_pack::data::output::count::objects::ObjectExpansion;

/// Expand `ids` into the entries a pack will carry. One call has one `seen` set, so a caller
/// combining two passes has to dedup by id itself — a repeated entry is a corrupt pack.
fn count_objects(
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

/// Stream `counts` as a v2 pack.
fn write_counts(
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

/// Expand `ids` under `expansion` and stream them as a pack.
fn pack_from_ids(
    odb: &gix_odb::Handle,
    ids: Vec<ObjectId>,
    expansion: ObjectExpansion,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    write_counts(odb, count_objects(odb, ids, expansion, interrupt)?, out, interrupt)
}
```

Update `write_pack_of` (upload.rs:743-755) to pass `ObjectExpansion::TreeContents` — a shallow boundary's parent is withheld, so a diff against it would be a delta onto nothing:

```rust
    pack_from_ids(odb, ids, ObjectExpansion::TreeContents, out, interrupt)
```

Replace the body of `write_pack` (upload.rs:758-798) after the doc comment:

```rust
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
    let (mut tips, mut tags, mut leaves) = (Vec::new(), Vec::new(), Vec::new());
    for w in &wants {
        let mut id = *w;
        loop {
            match gix_object::FindExt::find(odb, &id, &mut buf)?.decode()? {
                gix_object::ObjectRef::Commit(_) => {
                    tips.push(id);
                    break;
                }
                gix_object::ObjectRef::Tag(t) => {
                    tags.push(id);
                    id = t.target();
                }
                _ => {
                    leaves.push(id);
                    break;
                }
            }
        }
    }
    let mut commits = tags;
    for info in gix_traverse::commit::Simple::new(tips, odb.clone()).hide(haves)? {
        commits.push(info?.id);
    }
    // Commits carry only what they ADD over their parents: the client either has the parent
    // (it was a `have`) or is getting it in this same pack. Expanding every commit's whole tree
    // instead made an incremental fetch cost O(repo) — each `git fetch` re-sent every blob.
    // A tree or blob wanted by id (a promisor fetch) is still expanded whole, as git does; its
    // pass is deduped against the first because each count has its own `seen` set.
    let mut counts = count_objects(odb, commits, ObjectExpansion::TreeAdditionsComparedToAncestor, interrupt)?;
    if !leaves.is_empty() {
        let mut seen: std::collections::HashSet<ObjectId> = counts.iter().map(|c| c.id).collect();
        counts.extend(
            count_objects(odb, leaves, ObjectExpansion::TreeContents, interrupt)?
                .into_iter()
                .filter(|c| seen.insert(c.id)),
        );
    }
    write_counts(odb, counts, out, interrupt)
}
```

Leave `reachable_set_hiding` (upload.rs:688-694) on `TreeContents`: it answers "which objects does this repo have", which is the full set by definition.

- [ ] **Step 4: Run the test and the whole protocol/e2e surface**

Run: `cargo test --test protocol` then `cargo test --test http_e2e` then `cargo test --test ssh_e2e`
Expected: PASS, including `shallow_clone_deepen_and_unshallow`, `depth_across_a_merge_and_the_other_cutoffs` and `partial_clone_fetches_blobs_on_demand` (the promisor path exercises the `leaves` pass).

- [ ] **Step 5: Commit**

```bash
git add src/protocol/upload.rs tests/protocol.rs
git commit -m "Send only tree additions on an incremental fetch"
```

---

### Task 2: A filtered pack honours its filter

**Files:**
- Modify: `src/protocol/upload.rs:361-372` (the `(shallow, Some(f))` arm) and `write_pack_of`
- Test: `tests/http_e2e.rs`

**Context:** Found while reading Task 1. `filtered_objects` returns an EXPLICIT list (commits, kept trees, kept blobs), but it goes through `write_pack_of` → `pack_from_ids` with `TreeContents`, which expands every commit in that list back to its whole tree — the filter is computed and then discarded. The existing `partial_clone_fetches_blobs_on_demand` only checks that the clone works, which it does either way because git tolerates extra objects. The fix is `ObjectExpansion::AsIs` for the filtered path; with Task 1's parameter that is one argument.

**Interfaces:**
- Consumes: `pack_from_ids(odb, ids, expansion, out, interrupt)` from Task 1.

- [ ] **Step 1: Write the failing test**

Add to `partial_clone_fetches_blobs_on_demand` in `tests/http_e2e.rs`, right after the `blob:none` clone (before reading `f2.txt`, which triggers the lazy fetch):

```rust
    // The clone must actually LACK the blobs — `--missing=print` lists them without fetching.
    // A server that expands the filtered list back to whole trees passes every other check here.
    let missing = common::git(&none, &["rev-list", "--objects", "--missing=print", "HEAD"]);
    assert!(
        missing.lines().filter(|l| l.starts_with('?')).count() >= 3,
        "blob:none must leave the blobs behind; got:\n{missing}"
    );
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test http_e2e partial_clone_fetches_blobs_on_demand`
Expected: FAIL — zero `?` lines; every blob arrived with the clone.

- [ ] **Step 3: Send the filtered list as-is**

`write_pack_of` gains an `expansion` parameter so the two callers say what they mean:

```rust
pub(crate) fn write_pack_of(
    odb: &gix_odb::Handle,
    commits: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    expansion: ObjectExpansion,
    out: &mut dyn Write,
    interrupt: &AtomicBool,
) -> Result<()> {
    let have: std::collections::HashSet<ObjectId> = haves.into_iter().collect();
    let ids: Vec<ObjectId> = commits.into_iter().filter(|c| !have.contains(c)).collect();
    pack_from_ids(odb, ids, expansion, out, interrupt)
}
```

In `fetch`'s match (upload.rs:361-382):

```rust
        // A filtered pack is an explicit object list by construction — every object in it was
        // chosen one by one — so it goes out AS IS. Expanding it would put back exactly the
        // blobs the filter removed.
        (shallow, Some(f)) => {
            let commits = match shallow {
                Some(s) => s.commits.clone(),
                None => commit_range(&odb, wants.clone(), common.clone())?,
            };
            let mut ids = filtered_objects(&odb, &commits, f)?;
            ids.extend(extra_tags);
            write_pack_of(&odb, ids, common, ObjectExpansion::AsIs, &mut band, interrupt)
        }
        (Some(s), None) => {
            let mut ids = s.commits.clone();
            ids.extend(extra_tags);
            write_pack_of(&odb, ids, common, ObjectExpansion::TreeContents, &mut band, interrupt)
        }
```

Check `filtered_objects` already pushes the commit's root tree id when the filter is not `NoTrees` (it does: `trees.push(commit.tree())` then every subtree via the `while let` loop) — with `AsIs` nothing else will add it.

- [ ] **Step 4: Run tests**

Run: `cargo test --test http_e2e` then `cargo test --test protocol`
Expected: PASS — `tree:0` and `blob:limit=1k` clones in the same test still fsck clean.

- [ ] **Step 5: Commit**

```bash
git add src/protocol/upload.rs tests/http_e2e.rs
git commit -m "Send a filtered pack as the explicit list the filter chose"
```

---

### Task 3: Cap the incoming pack on every transport

**Files:**
- Modify: `src/protocol/receive.rs:334-365` (`write_pack`)
- Create: `tests/pack_cap.rs`

**Context:** HTTP enforces `max_body` in axum's extractor; SSH (`src/ssh.rs:256`) hands `receive::serve` a raw channel bridge with nothing in front, so an authenticated pusher can stream a pack until the node's disk is full. The cap belongs in `receive::write_pack`, the one place both transports feed the pack through — HTTP gets a second, identical cap for free. A `Take` would hand the indexer a truncated stream and a confusing "pack truncated" error; a reader that ERRORS past the cap tells the pusher what happened.

- [ ] **Step 1: Write the failing test**

Create `tests/pack_cap.rs` (its own file because it sets a process-global env var):

```rust
//! Its own binary: `KLOUDLITE_MAX_BODY` is process-global and every other push test would
//! trip over a 1 KiB cap.
mod common;
use kloudlite::pktline;
use kloudlite::protocol::receive;
use std::io::{Cursor, Write};

fn pack_of(dir: &std::path::Path, revs: &str) -> Vec<u8> {
    use std::process::{Command, Stdio};
    let mut c = Command::new("git")
        .args(["pack-objects", "--stdout", "--revs", "-q"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(revs.as_bytes()).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(out.status.success());
    out.stdout
}

/// The SSH path has no HTTP body limit in front of it; the pack reader itself must refuse.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_pack_is_refused_before_it_is_indexed() {
    if !common::have_git() {
        eprintln!("skip: no git");
        return;
    }
    std::env::set_var("KLOUDLITE_MAX_BODY", "1024");
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("a", "r").await.unwrap();

    let d = tempfile::tempdir().unwrap();
    common::git(d.path(), &["init", "-q", "-b", "main"]);
    // Incompressible, so the pack is comfortably past 1 KiB.
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut body = Vec::with_capacity(8192);
    while body.len() < 8192 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        body.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(d.path().join("big.bin"), &body).unwrap();
    common::git(d.path(), &["add", "."]);
    common::git(d.path(), &["commit", "-qm", "big"]);
    let head = common::git(d.path(), &["rev-parse", "HEAD"]);
    let pack = pack_of(d.path(), &format!("{head}\n"));
    assert!(pack.len() > 1024);

    let mut req = Vec::new();
    pktline::write_pkt(
        &mut req,
        format!("{} {head} refs/heads/main\0report-status", "0".repeat(40)).as_bytes(),
    )
    .unwrap();
    pktline::write_flush(&mut req).unwrap();
    req.extend(pack);

    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let s2 = s.clone();
    let resp = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        receive::serve(&s2, &repo, &mut Cursor::new(req), &mut out, &Default::default()).map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8_lossy(&resp).to_string();
    assert!(text.contains("unpack error"), "the push must be refused: {text}");
    assert!(text.contains("size limit"), "and say why: {text}");
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(s.get_ref(&repo, "refs/heads/main").await.unwrap().is_none());
    assert_eq!(
        std::fs::read_dir(&repo.pack_dir).unwrap().count(),
        0,
        "nothing of the refused pack stays on disk"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test pack_cap`
Expected: FAIL — `unpack ok`, the ref is set, the 8 KiB pack was indexed.

- [ ] **Step 3: Add the capped reader**

In `src/protocol/receive.rs`, above `write_pack`:

```rust
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
            return Err(std::io::Error::other("pack exceeds the size limit"));
        }
        let n = (b.len() as u64).min(self.left) as usize;
        Ok(&b[..n])
    }
    fn consume(&mut self, n: usize) {
        self.left -= n as u64;
        self.inner.consume(n);
    }
}
```

In `write_pack`, replace the `input` argument to `write_to_directory`:

```rust
    let outcome = gix_pack::Bundle::write_to_directory(
        Capped { inner: input, left: crate::http::max_body() as u64 },
        Some(&repo.pack_dir),
        &mut progress,
        should_interrupt,
        Some(odb),
        opts,
    )?;
```

`crate::http::max_body` is `pub(crate)` (http.rs:22) — reachable from here.

- [ ] **Step 4: Run tests**

Run: `cargo test --test pack_cap` then `cargo test --test protocol` then `cargo test --test ssh_e2e`
Expected: PASS. If the `read_dir(...).count() == 0` assertion fails because gix left a partial file, note which name survives: `write_to_directory` writes to a temp and only renames on success, so a survivor is a bug worth a `ponytail:` note rather than a loosened assert.

- [ ] **Step 5: Commit**

```bash
git add src/protocol/receive.rs tests/pack_cap.rs
git commit -m "Cap the incoming pack size on SSH as well as HTTP"
```

---

## MEDIUM

### Task 4: A database evicted during its own open is closed, not leaked

**Files:**
- Modify: `src/pool.rs:206-241` (`get_once`)
- Test: `src/pool.rs` inline tests

**Context:** `evict` on an entry whose `OnceCell` is still empty finds no handle (`shared.db.get()` is `None`) and removes the slot. The open then completes, `get_once` hands the handle to its caller, and nothing names it: no sweep can close it, so it holds the writer epoch until the process dies — and fences whichever node the lease went to. Re-check that the slot is still ours after the open; if not, close the fresh handle and report a fence, because an evict mid-open means this node lost the lease and the caller must re-route.

**Interfaces:**
- Produces: `async fn adopt(&self, key: &str, entry: &Arc<Entry>, handle: Arc<Db>) -> Result<Arc<Db>>` (private; tests in the module reach it).

- [ ] **Step 1: Write the failing test**

In `src/pool.rs` `mod tests`:

```rust
    /// An evict that lands while the open is in flight removes a slot with no handle in it. The
    /// open then finishes and, before this fix, the handle was returned with no map entry naming
    /// it: never swept, never closed, holding the writer epoch for the life of the process.
    #[tokio::test]
    async fn a_handle_whose_slot_was_evicted_mid_open_is_closed() {
        let p = pool();
        let entry = Arc::new(Entry {
            db: tokio::sync::OnceCell::new(),
            last_used: Mutex::new(Instant::now()),
            last_flush: Mutex::new(Instant::now()),
            releasing: AtomicBool::new(false),
        });
        // Deliberately NOT in the map: the shape an evict leaves behind.
        let db = entry.db.get_or_try_init(|| p.open("alice", "web")).await.unwrap().clone();
        let err = p.adopt("alice/web", &entry, db.clone()).await.unwrap_err();
        assert!(is_fenced(&err), "the caller must re-route: {err}");
        assert!(db.status().close_reason.is_some(), "the orphaned handle must be closed");
        assert_eq!(p.warm_count(), 0);
        // And the slot that IS in the map is adopted as before.
        let live = p.get("alice", "web").await.unwrap();
        assert!(live.status().close_reason.is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib pool::tests::a_handle_whose_slot_was_evicted_mid_open_is_closed`
Expected: FAIL to compile — no `adopt`.

- [ ] **Step 3: Add `adopt` and call it from `get_once`**

```rust
    /// The last step of an open: keep the handle only if the map still names this slot.
    ///
    /// An evict that ran DURING the open (a lost lease, a fence) found no handle to close and
    /// removed the slot. Adopting the handle now would leave a database open that nothing names
    /// and no sweep can reach — holding the writer epoch until the process dies, which is the
    /// fence the next owner will hit. Close it and report a fence: the caller re-routes.
    async fn adopt(&self, key: &str, entry: &Arc<Entry>, handle: Arc<Db>) -> Result<Arc<Db>> {
        let current = self.entries.lock().unwrap().get(key).is_some_and(|e| Arc::ptr_eq(e, entry));
        if current {
            return Ok(handle);
        }
        let _ = handle.close().await;
        Err(FencedError { repo: key.to_string() }.into())
    }
```

In `get_once`, replace the `.clone();` / `enforce_bound` tail (pool.rs:229-240):

```rust
        let handle = entry
            .db
            .get_or_try_init(|| self.open(owner, name))
            .await
            // A failed open leaves an empty cell, so the next caller retries rather than
            // inheriting the error. Drop the slot so a poisoned key cannot accumulate.
            .inspect_err(|_| {
                self.entries.lock().unwrap().remove(&key);
            })?
            .clone();
        let handle = self.adopt(&key, &entry, handle).await?;
        self.enforce_bound().await;
        Ok(handle)
```

- [ ] **Step 4: Run the pool tests**

Run: `cargo test --lib pool`
Expected: PASS. If `db.status().close_reason` stays `None` after `close()` on this SlateDB version, assert instead that a second `Db::builder(path("alice","web"), os).build()` then `put` succeeds without the first handle being fenced-reported — and say so in the test comment.

- [ ] **Step 5: Commit**

```bash
git add src/pool.rs
git commit -m "Close a database whose pool slot was evicted during its open"
```

---

### Task 5: Only a fence is reported as a fence

**Files:**
- Modify: `src/pool.rs:196-204` (`get`)
- Test: `src/pool.rs` inline tests

**Context:** `get` treats ANY `close_reason` as fenced. A handle closed `Clean` (shutdown racing a request) or `Panic` (a background task died) gets reported as a fence, which sends the caller through re-route → force-claim → possibly stealing the repo from a healthy peer. Nobody else holds the epoch in those cases; the right answer is "dead handle, dropped, retry".

- [ ] **Step 1: Write the failing test**

```rust
    /// A handle that was closed for any reason but a fence is dead, not stolen: report a plain
    /// error (the next call reopens) rather than a fence (the caller would re-route and
    /// force-claim a repo nobody took).
    #[tokio::test]
    async fn a_cleanly_closed_handle_is_not_reported_as_fenced() {
        let p = pool();
        let h = p.get("alice", "web").await.unwrap();
        h.close().await.unwrap();
        drop(h);
        let e = p.get("alice", "web").await.unwrap_err();
        assert!(!is_fenced(&e), "clean close reported as a fence: {e}");
        assert_eq!(p.warm_count(), 0, "the dead handle is dropped");
        p.get("alice", "web").await.unwrap().put(b"k", b"v").await.unwrap();
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib pool::tests::a_cleanly_closed_handle_is_not_reported_as_fenced`
Expected: FAIL on `!is_fenced`.

- [ ] **Step 3: Match the reason**

```rust
    pub async fn get(self: &Arc<Self>, owner: &str, name: &str) -> Result<Arc<Db>> {
        let h = self.get_once(owner, name).await?;
        match h.status().close_reason {
            None => Ok(h),
            Some(slatedb::CloseReason::Fenced) => {
                self.evict_if_same(owner, name, &h).await;
                drop(h);
                Err(FencedError { repo: format!("{owner}/{name}") }.into())
            }
            // Closed clean (a shutdown racing this request) or by a panicked background task:
            // nobody else holds the epoch, so this is not a routing question and must not be
            // answered as one — a fence here sends the caller off to force-claim a repo nobody
            // took. Drop the dead handle; the next call reopens in place.
            Some(_) => {
                self.evict_if_same(owner, name, &h).await;
                drop(h);
                Err(crate::err(format!("{owner}/{name}: database was closed; retry")))
            }
        }
    }
```

`CloseReason` is `#[non_exhaustive]`-free but has three variants; the `Some(_)` arm covers `Clean` and `Panic`.

- [ ] **Step 4: Run the pool tests**

Run: `cargo test --lib pool`
Expected: PASS, including `fenced_handle_is_evicted_and_reported`.

- [ ] **Step 5: Commit**

```bash
git add src/pool.rs
git commit -m "Report only a fenced close as a fence"
```

---

### Task 6: `open_repo` prunes local packs the index no longer names

**Files:**
- Modify: `src/store.rs:226-269` (`open_repo`)
- Test: `tests/store.rs`

**Context:** The local cache is only ever added to. After a repo moves away, is repacked there, and moves back, the superseded `.pack/.idx` pairs are still in `pack_dir`: gix-odb discovers packs by `.idx`, so objects the repack dropped stay servable, and the disk is never reclaimed. Prune what the index does not name — but only files older than an hour, because a push in flight has written its pack locally and not yet uploaded/recorded it.

- [ ] **Step 1: Write the failing test**

Add to `tests/store.rs`:

```rust
/// After a repo is repacked elsewhere and comes back, the superseded packs are still in this
/// node's cache — servable by gix-odb and never reclaimed. `open_repo` must drop what the index
/// no longer names, but not a fresh pack a push may still be uploading.
#[tokio::test]
async fn open_repo_prunes_packs_the_index_no_longer_names() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
    for f in ["pack-stale.pack", "pack-stale.idx", "pack-fresh.pack", "pack-fresh.idx"] {
        let p = repo.pack_dir.join(f);
        std::fs::write(&p, b"x").unwrap();
        if f.contains("stale") {
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(old).unwrap();
        }
    }
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    assert!(!repo.pack_dir.join("pack-stale.pack").exists(), "stale pack pruned");
    assert!(!repo.pack_dir.join("pack-stale.idx").exists(), "stale idx pruned");
    assert!(repo.pack_dir.join("pack-fresh.pack").exists(), "a pack a push may still be uploading is kept");
    assert!(repo.pack_dir.join("pack-fresh.idx").exists());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test store open_repo_prunes_packs_the_index_no_longer_names`
Expected: FAIL — the stale files are still there.

- [ ] **Step 3: Prune after the fetch**

In `open_repo`, after the `for batch in [packs, idxs]` loop and before `Ok(Some(repo))`:

```rust
        prune_stale_packs(&repo.pack_dir, &files)?;
```

`files` is moved into the partition above; take the names first. Change the partition to work on a clone: `let (packs, idxs): (Vec<_>, Vec<_>) = files.clone().into_iter().partition(...)`. Then add, as a free function near `pack_index_prefix`:

```rust
/// Remove local pack files the index no longer names.
///
/// The cache was only ever added to. After a repo moves away, is repacked there and moves back,
/// the superseded packs are still here: gix-odb discovers packs by `.idx`, so objects the repack
/// dropped stay servable, and the disk is never reclaimed. Only files past `STALE_AFTER` go — a
/// push in flight has written its pack locally and not yet uploaded or recorded it, and must not
/// lose it underneath. `.idx` first, as everywhere: no reader sees an index without its data.
// ponytail: an mtime guard, not a lock; a single push whose upload takes over an hour would lose
// its pack here. Track in-flight packs explicitly if uploads ever get that slow.
fn prune_stale_packs(pack_dir: &Path, indexed: &[(String, u64)]) -> std::io::Result<()> {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);
    let now = std::time::SystemTime::now();
    let mut stale: Vec<PathBuf> = Vec::new();
    for ent in std::fs::read_dir(pack_dir)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let is_pack = name.starts_with("pack-") && (name.ends_with(".pack") || name.ends_with(".idx"));
        if !is_pack || indexed.iter().any(|(f, _)| *f == name) {
            continue;
        }
        let old = ent
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if old {
            stale.push(ent.path());
        }
    }
    stale.sort_by_key(|p| p.extension().and_then(|x| x.to_str()) != Some("idx"));
    for p in stale {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}
```

- [ ] **Step 4: Run the store tests and the push path**

Run: `cargo test --test store` then `cargo test --test protocol`
Expected: PASS — `pack_sync_roundtrip` and every push test (their packs are fresh, so kept).

- [ ] **Step 5: Commit**

```bash
git add src/store.rs tests/store.rs
git commit -m "Prune cached packs the index no longer names"
```

---

### Task 7: Stream a pack download to disk

**Files:**
- Modify: `src/store.rs:327-351` (`fetch_pack_file`)
- Test: `tests/store.rs::pack_sync_roundtrip` (existing; it is the regression guard)

**Context:** `.bytes().await?` buffers the whole pack in RAM, and `open_repo` runs eight of these at once — a repo with a few 500 MiB packs costs gigabytes per open. `GetResult::into_stream()` plus `tokio_util::io::StreamReader` (the `io` feature is already on in `Cargo.toml`) pipes it straight to the temp file.

- [ ] **Step 1: Confirm the guard passes today**

Run: `cargo test --test store pack_sync_roundtrip`
Expected: PASS (it must stay green through the refactor).

- [ ] **Step 2: Replace the buffer with a copy**

```rust
    /// Download one pack file unless an identically sized copy is already cached.
    async fn fetch_pack_file(&self, repo: &Repo, fname: String, size: u64) -> Result<()> {
        let pack_dir = &repo.pack_dir;
        let local = pack_dir.join(&fname);
        if local.metadata().map(|m| m.len() == size).unwrap_or(false) {
            return Ok(());
        }
        let key = OsPath::from(format!("{}/{}", repo.s3_prefix(), fname));
        // Streamed, never buffered: `open_repo` runs eight of these at once, and a whole pack in
        // memory per download is gigabytes for a repo with a few large packs.
        let stream = self.os.get(&key).await?.into_stream().map_err(std::io::Error::other);
        let mut reader = tokio_util::io::StreamReader::new(stream);
        // unique per process+call: concurrent opens must not share a temp path
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = pack_dir.join(format!(".{fname}.{}.{seq}.tmp", std::process::id()));
        // fsync the data before the rename: otherwise a host crash can leave a renamed file with
        // the right length but unwritten contents, and the size-only skip above would then serve
        // that corrupt pack forever without re-fetching.
        {
            let mut w = tokio::fs::File::create(&tmp).await?;
            tokio::io::copy(&mut reader, &mut w).await?;
            w.sync_all().await?;
        }
        tokio::fs::rename(&tmp, &local).await?;
        Ok(())
    }
```

`TryStreamExt` is already imported at the top of the file (`use futures::{StreamExt, TryStreamExt}`), which provides `map_err`.

- [ ] **Step 3: Run tests**

Run: `cargo test --test store` then `cargo test --test protocol receive_then_fetch`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/store.rs
git commit -m "Stream pack downloads to disk instead of buffering them"
```

---

### Task 8: Cut the signature payload from the raw commit bytes

**Files:**
- Modify: `src/browse.rs:359-394` (`Signed`, `signature_of`)
- Test: `src/browse.rs` inline tests (new `#[cfg(test)] mod tests`)

**Context:** The payload git signed is the raw commit with the `gpgsig` header (and its space-continued lines) removed. `signature_of` rebuilds it by re-serialising through gix, which normalises what it parsed: a `-0000` zone comes back `+0000` (verified: `gix-date-0.15.6/src/time/write.rs:33` writes the sign from the offset's sign, so `-0000` cannot round-trip). Any such commit reads `Invalid` for no visible reason. Cut the header out of the bytes instead.

- [ ] **Step 1: Write the failing test**

Add at the bottom of `src/browse.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::without_gpgsig;

    /// git signed the raw bytes minus the `gpgsig` header; a payload rebuilt by re-serialising is
    /// not those bytes whenever gix normalises something it parsed — here the `-0000` zone.
    #[test]
    fn signature_payload_is_cut_from_the_raw_bytes() {
        let raw: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author t <t@t> 0 -0000\n\
committer t <t@t> 0 -0000\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n -----END PGP SIGNATURE-----\n\
\n\
msg\n";
        let want: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author t <t@t> 0 -0000\n\
committer t <t@t> 0 -0000\n\
\n\
msg\n";
        assert_eq!(without_gpgsig(raw), want);

        // The re-serialising approach this replaces cannot produce `want`.
        use gix_object::WriteTo;
        let parsed = gix_object::CommitRef::from_bytes(raw, gix_hash::Kind::Sha1).unwrap();
        if let Ok(mut owned) = parsed.to_owned() {
            owned.extra_headers.retain(|(name, _)| name.as_slice() != b"gpgsig");
            let mut rebuilt = Vec::new();
            owned.write_to(&mut rebuilt).unwrap();
            assert_ne!(rebuilt, want, "if these are equal the fixture no longer proves anything");
        }
    }

    #[test]
    fn a_commit_without_a_signature_is_unchanged() {
        let raw: &[u8] = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\nmsg\n";
        assert_eq!(without_gpgsig(raw), raw);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib browse::tests`
Expected: FAIL to compile — no `without_gpgsig`.

- [ ] **Step 3: Implement the cut and use it**

Replace `signature_of` (browse.rs:373-394) and fix the `Signed` doc that says the payload "has to be rebuilt":

```rust
/// A commit's signature, and the bytes it signs.
///
/// Git signs the commit object with its `gpgsig` header removed — so the payload is the raw
/// bytes with that header cut out, never a re-serialisation. Returning both means the verifier
/// never has to know how a commit is laid out.
pub struct Signed {
    /// The armoured signature: an OpenPGP block, or an SSH `SSHSIG` block.
    pub signature: String,
    /// Exactly the bytes the signature covers.
    pub payload: Vec<u8>,
    /// From the commit itself, for checking the signer is who the commit claims.
    pub author_email: String,
}

pub fn signature_of(odb: &gix_odb::Handle, oid: ObjectId) -> Result<Option<Signed>> {
    let mut buf = Vec::new();
    let data = odb.find(&oid, &mut buf).map_err(find_err)?;
    if data.kind != gix_object::Kind::Commit {
        return Err(nf(format!("{oid} is a {}, not a commit", data.kind)));
    }
    let commit = gix_object::CommitRef::from_bytes(data.data, oid.kind())?;
    let Some(sig) = commit.extra_headers().find("gpgsig") else {
        return Ok(None);
    };
    let signature = sig.to_string();
    let author_email = commit.author().map(|a| a.email.to_string()).unwrap_or_default();
    Ok(Some(Signed { signature, payload: without_gpgsig(data.data), author_email }))
}

/// The raw commit with the `gpgsig` header and its continuation lines cut out — exactly what git
/// hashed when it signed. Cut, not re-serialised: gix normalises what it parses (a `-0000` zone
/// comes back `+0000`), and a payload that is not byte-for-byte the original makes a perfectly
/// good signature read as invalid.
fn without_gpgsig(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut rest = raw;
    let mut in_sig = false;
    while !rest.is_empty() {
        let end = rest.iter().position(|&b| b == b'\n').map_or(rest.len(), |p| p + 1);
        let line = &rest[..end];
        // The blank line ends the headers; the message after it travels verbatim.
        if line == b"\n" {
            out.extend_from_slice(rest);
            break;
        }
        if line.starts_with(b"gpgsig ") {
            in_sig = true;
        } else if in_sig && line.starts_with(b" ") {
            // a continuation line of the signature
        } else {
            in_sig = false;
            out.extend_from_slice(line);
        }
        rest = &rest[end..];
    }
    out
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib browse` then `cargo test --test browse` then `cargo test --test api_server`
Expected: PASS — the `signatures` api tests still verify (they build payloads through this function).

- [ ] **Step 5: Commit**

```bash
git add src/browse.rs
git commit -m "Cut the signed payload from the raw commit bytes"
```

---

### Task 9: Index merge objects off the runtime thread, under a unique temp name

**Files:**
- Modify: `src/objects.rs:47-53, 106, 114-155` (`Staging::write`, `write_commit`, `write_pack_of_objects`)
- Test: `tests/protocol.rs::squash_and_merge_commit_land_the_right_shape` (existing; regression guard) + one new unit test for the temp name

**Context:** Two findings in one function. (a) `write_pack_of_objects` is `async` but does all its work synchronously — writes a pack, runs `Bundle::write_to_directory` (zlib, SHA-1, index) — on a runtime worker thread. (b) The temp pack is named by content hash, so two merges of identical staged content race on the same file: one deletes it while the other is still reading it. Move the sync part into `spawn_blocking` and name the temp per process+call, the same way `store.rs::fetch_pack_file` does. The temp is only INPUT to the indexer (which names its own output by checksum and handles a duplicate), so no rename is needed.

- [ ] **Step 1: Write the failing test**

In `src/objects.rs`, add to the existing `#[cfg(test)]` area (a new module is fine):

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib objects::temp_name_tests`
Expected: FAIL to compile — no `incoming_pack_path`.

- [ ] **Step 3: Split the sync work out and name the temp uniquely**

Replace `write_pack_of_objects` (objects.rs:110-155):

```rust
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
```

Update the two callers to pass ownership:

```rust
    // Staging::write
    write_pack_of_objects(store, repo, self.objects).await
```
```rust
    // write_commit
    write_pack_of_objects(store, repo, vec![(gix_object::Kind::Commit, body)]).await?;
```

`repo.clone()` works: `Repo` derives `Clone` (store.rs:92).

- [ ] **Step 4: Run tests**

Run: `cargo test --lib objects` then `cargo test --test protocol squash_and_merge_commit_land_the_right_shape` then `cargo test --test pulls`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/objects.rs
git commit -m "Index merge objects on a blocking thread under a per-call temp name"
```

---

### Task 10: Compute protection verdicts on a blocking thread

**Files:**
- Modify: `src/refs.rs:17-21` (`RefUpdate`), `:339-368` (`protection_verdict`), `:415-476` (`update_refs`)
- Test: `tests/store.rs` (existing protection tests are the guard)

**Context:** `is_ancestor` walks up to 50,000 commits, synchronously, inside `update_refs` — an async fn awaited from the api tier's merge handler and from `receive.rs` via `block_on`. Compute every verdict in one `spawn_blocking` before the transaction opens; the verdicts depend only on the rules, the odb and the updates. `protection_verdict` never used `self`, so it becomes a free function.

- [ ] **Step 1: Confirm the guard passes today**

Run: `cargo test --test store` (the `protections` tests at `tests/store.rs:330-400`)
Expected: PASS.

- [ ] **Step 2: Make `RefUpdate` clonable and lift the verdicts**

```rust
#[derive(Clone)]
pub struct RefUpdate {
    pub name: String,
    pub old: Option<ObjectId>,
    pub new: Option<ObjectId>,
}
```

Turn `protection_verdict` into a free function (drop `&self`, drop the `impl Store` placement — move it next to `is_ancestor`):

```rust
/// `Some(reason)` if a rule refuses this update. The reason is shown to the
/// person pushing, so it says which rule and which branch.
fn protection_verdict(rules: &[Protection], odb: Option<&gix_odb::Handle>, u: &RefUpdate) -> Option<String> {
    // body unchanged
}
```

In `update_refs`, replace the rules/odb/loop head:

```rust
        let txn = self
            .db_for(&repo.owner, &repo.name).await?
            .begin(IsolationLevel::SerializableSnapshot)
            .await?;
        let mut results = Vec::with_capacity(updates.len());
        let mut any_rejected = false;

        // Enforced HERE rather than in the push path, so ssh and http and every
        // future caller are covered by one check — the same reasoning as the cache
        // invalidation below. Loaded once per batch; a repo with no rules pays one
        // empty scan. The verdicts are decided on a blocking thread: a no-force rule
        // walks up to `ANCESTRY_BUDGET` commits, which is not work for a runtime
        // worker that every other request on this node shares.
        let rules = self.protections(&repo.owner, &repo.name).await?;
        let verdicts: Vec<Option<String>> = if rules.is_empty() {
            vec![None; updates.len()]
        } else {
            let odb = repo.odb().ok();
            let ups: Vec<RefUpdate> = updates.to_vec();
            tokio::task::spawn_blocking(move || {
                ups.iter().map(|u| protection_verdict(&rules, odb.as_ref(), u)).collect()
            })
            .await?
        };

        for (u, verdict) in updates.iter().zip(verdicts) {
            if let Some(reason) = verdict {
                results.push(Some(reason));
                any_rejected = true;
                continue;
            }
            // rest of the loop body unchanged
```

`Protection` already derives `Clone`; `gix_odb::Handle` is `Send`.

- [ ] **Step 3: Run tests**

Run: `cargo test --test store` then `cargo test --test protocol` then `cargo test --lib refs`
Expected: PASS. `tests/store.rs` uses `#[tokio::test]` (current-thread); `spawn_blocking` is fine there, unlike `block_in_place`.

- [ ] **Step 4: Commit**

```bash
git add src/refs.rs
git commit -m "Decide protection verdicts on a blocking thread"
```

---

### Task 11: Walk the repo once per fetch

**Files:**
- Modify: `src/protocol/upload.rs:259-302, 336-353` (`fetch`)
- Test: existing `tests/protocol.rs` and `tests/http_e2e.rs` fetch tests (regression guards)

**Context:** `reachable_set` is a full-repo walk and `fetch` runs it up to three times: for the `have` check (:265), for non-tip `want`s (:297), and for `include-tag` (:342, which walks the SENT set with `reachable_set_hiding`). Memoize the first two — the same question — and answer the third from the commit list, which is what `include-tag` actually needs (a tag's target is a commit in practice).

- [ ] **Step 1: Add a memo and use it twice**

Add above `reachable_set`:

```rust
/// `reachable_set`, computed at most once per fetch. Both the `have` check and the non-tip
/// `want` check ask "what does this repo have", and it is a walk of the whole repo.
fn ours<'a>(
    slot: &'a mut Option<std::collections::HashSet<ObjectId>>,
    odb: &gix_odb::Handle,
    tips: &[ObjectId],
) -> Result<&'a std::collections::HashSet<ObjectId>> {
    if slot.is_none() {
        *slot = Some(reachable_set(odb, tips.to_vec())?);
    }
    Ok(slot.as_ref().expect("just filled"))
}
```

In `fetch`, replace :259-268 and :295-302:

```rust
    // A `have` counts as common only if it is reachable from THIS repo's refs. Testing raw
    // existence in the local odb would answer for packs a rejected push left behind, or
    // packs a repack elsewhere has since dropped.
    let mut have_set: Option<std::collections::HashSet<ObjectId>> = None;
    let common: Vec<ObjectId> = if haves.is_empty() {
        Vec::new()
    } else {
        let ours = ours(&mut have_set, &odb, &tips)?;
        haves.iter().copied().filter(|h| ours.contains(h)).collect()
    };
```
```rust
    let unknown: Vec<ObjectId> = wants.iter().copied().filter(|w| !tips.contains(w)).collect();
    if !unknown.is_empty() {
        let ours = ours(&mut have_set, &odb, &tips)?;
        if let Some(w) = unknown.iter().find(|w| !ours.contains(*w)) {
            pktline::write_text(out, &format!("ERR upload-pack: not our ref {}", w.to_hex()))?;
            return Ok(());
        }
    }
```

Delete the stale line `// ponytail: no ref-in-want, no include-tag; add if clients complain` (:268) — both are implemented.

- [ ] **Step 2: Answer `include-tag` from the commit list**

Replace :336-343:

```rust
    // Decided from the commits being sent, not a second walk of every object: a tag names a
    // commit in practice, and `commit_range` is O(commits) where the object walk is O(repo).
    // ponytail: a tag pointing straight at a tree or blob is not carried by include-tag; the
    // client fetches it by name on the next `git fetch --tags`.
    let mut extra_tags: Vec<ObjectId> = Vec::new();
    if include_tag {
        let sending: std::collections::HashSet<ObjectId> = match &shallow {
            Some(s) => s.commits.iter().copied().collect(),
            None => commit_range(&odb, wants.clone(), common.clone())?.into_iter().collect(),
        };
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test protocol` then `cargo test --test http_e2e`
Expected: PASS — `clone_push_fetch` (tags), the promisor test, and `an_object_no_ref_reaches_is_refused`.

- [ ] **Step 4: Commit**

```bash
git add src/protocol/upload.rs
git commit -m "Walk the repo at most once per fetch"
```

---

### Task 12: Decide "too large to diff" from the header, before inflating

**Files:**
- Modify: `src/browse.rs:10-12, 521-571` (`diff_trees_inner`)
- Test: `tests/browse.rs`

**Context:** The 4 MiB ceiling is checked between files, after BOTH blobs of the current file were fully inflated — one 200 MiB blob blows the cap the cap exists for. A blob's size is in its header (`try_header`, already used by `with_sizes`); refuse before reading.

- [ ] **Step 1: Write the failing test**

Add to `tests/browse.rs`:

```rust
/// A blob past the diff ceiling is refused from its HEADER, never inflated: inflating it to find
/// out is the memory cliff the ceiling exists to avoid.
#[tokio::test(flavor = "multi_thread")]
async fn a_huge_blob_is_not_inflated_to_be_refused() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "fat", |c| {
        std::fs::write(c.join("small.txt"), "one\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "one"]);
        // 5 MiB of text, past the 4 MiB ceiling.
        std::fs::write(c.join("big.txt"), "x".repeat(5 * 1024 * 1024)).unwrap();
        std::fs::write(c.join("small.txt"), "two\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "two"]);
    })
    .await;
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let (_, diff) = browse::commit(&odb, head).unwrap();
    assert!(diff.contains(browse::TOO_LARGE_MARKER), "big.txt is marked, not diffed: {}", &diff[..diff.len().min(400)]);
    assert!(diff.contains("-one\n+two\n"), "the small file is still diffed: {diff}");
    assert!(diff.len() < 1024, "nothing of the big blob is in the output: {}", diff.len());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test browse a_huge_blob_is_not_inflated_to_be_refused`
Expected: FAIL to compile (no `TOO_LARGE_MARKER`); after stubbing the constant, FAIL on `diff.len() < 1024` — the 5 MiB file is diffed in full and then the whole diff truncated.

- [ ] **Step 3: Check the header first**

Add next to `BINARY_MARKER`:

```rust
/// What a diff says for a blob past `MAX_DIFF`. Decided from the object header, so the blob is
/// never inflated to learn it cannot be shown.
pub const TOO_LARGE_MARKER: &str = "File too large to diff";
```

In `diff_trees_inner`, between the `diff.len() >= MAX_DIFF` check and `let bytes = ...`:

```rust
        // From the header, never by inflating: a blob past the ceiling cannot be shown anyway,
        // and reading it to find that out is the memory cliff the ceiling exists to avoid.
        let too_big = |id: Option<ObjectId>| -> bool {
            use gix_object::FindHeader;
            id.and_then(|id| odb.try_header(&id).ok().flatten())
                .is_some_and(|h| h.size > MAX_DIFF as u64)
        };
        if too_big(old) || too_big(new) {
            diff.push_str(&format!("--- a/{path}\n+++ b/{path}\n{TOO_LARGE_MARKER}\n"));
            continue;
        }
```

Update the `// ponytail: 4 MiB ceiling on the whole diff, checked between files...` comment to say the per-file check is now the header read above, and that streaming per file is still the upgrade path.

- [ ] **Step 4: Run tests**

Run: `cargo test --test browse`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/browse.rs tests/browse.rs
git commit -m "Refuse oversized blobs from the header before diffing"
```

---

### Task 13: One round trip per cache read

**Files:**
- Modify: `src/cache.rs:131-139` (`get`)
- Test: `src/cache.rs` inline tests

**Context:** `get` is `GET gen:{repo}` then `GET v1:{gen}:{repo}:{suffix}` — two RTTs on every api-tier read. The second key depends on the first value, so a pipeline cannot do it; a server-side script can. `redis::Script` (the `script` feature is in redis 0.27's defaults and `Cargo.toml` does not disable them) sends `EVALSHA`, falling back to `EVAL` once. The in-memory path and the "generation read error → skip" contract are untouched: a script error is a `None`.

- [ ] **Step 1: Write the failing test**

```rust
    /// One round trip per read: the generation and the entry are fetched by one server-side
    /// script. The stub answers the script call with a body; two sequential GETs would never
    /// see it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_is_one_script_call() {
        let c = scripted_cache_for_test(&[("EVAL", b"$4\r\nbody\r\n")]).await;
        assert_eq!(c.get("alice/web", "refs").await.as_deref(), Some(&b"body"[..]));
    }
```

(`"EVAL"` matches the `EVALSHA` the client sends first.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cache::tests::a_read_is_one_script_call`
Expected: FAIL — `get` issues `GET`, which the stub answers `+OK`, and returns `None`.

- [ ] **Step 3: Read through a script**

Add near `KEY_VERSION`:

```rust
/// Generation and entry in one server round trip. The entry's key depends on the generation's
/// value, so a pipeline cannot express this and two sequential GETs were two RTTs on every read.
/// A missing generation is `'0'` here for the same reason `generation()` says zero — see its doc.
static GET_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        "local g = redis.call('GET', 'gen:' .. ARGV[1]) or '0'\n\
         return redis.call('GET', ARGV[2] .. ':' .. g .. ':' .. ARGV[1] .. ':' .. ARGV[3])",
    )
});
```

Replace `get`:

```rust
    pub async fn get(&self, repo: &str, suffix: &str) -> Option<Vec<u8>> {
        if let Some(m) = &self.mem {
            let gen = self.generation(repo).await?;
            return mem_get(m, &key(gen, repo, suffix));
        }
        let mut c = self.conn.clone()?;
        // A script error (the generation unreadable, a timeout) is a miss, exactly as a failed
        // `generation()` read is: never a guessed generation.
        // Bound first: `arg` returns a borrow of the invocation, and a chained temporary would
        // be dropped before the future that borrows it is awaited.
        let mut call = GET_SCRIPT.prepare_invoke();
        call.arg(repo).arg(KEY_VERSION).arg(suffix);
        let fut = call.invoke_async::<Option<Vec<u8>>>(&mut c);
        tokio::time::timeout(CMD_TIMEOUT, fut).await.ok()?.ok().flatten()
    }
```

The keys are passed as `ARGV`, not `KEYS`, on purpose: the script touches two keys derived from one name, and declaring one of them as `KEYS[1]` would be a lie to a cluster router this deployment does not have.

- [ ] **Step 4: Run the cache tests**

Run: `cargo test --lib cache`
Expected: PASS — `the_first_purge_orphans_what_was_cached` (mem path), `generation_error_disables_cache_not_defaults_to_zero` (the stub errors `EVALSHA`? no — it errors commands containing `GET`, and the script's `EVAL` body contains `GET`, so the fallback errors and `get` is `None`; the `EVALSHA` form returns `+OK`, which does not parse as `Option<Vec<u8>>` and is also `None`). If that test's `put` half now reaches Redis with a stubbed `+OK`, it still wrote under no generation — assert stays green.

- [ ] **Step 5: Commit**

```bash
git add src/cache.rs
git commit -m "Fetch a cache entry and its generation in one round trip"
```

---

## LOW

### Task 14: Delete the stale ownership-map paragraphs

**Files:**
- Modify: `src/ownership.rs:227-263, 388-389`

**Context:** `leader_settings` (ownership.rs:189-224) turns compaction ON and leaves the default collectors for manifest/compacted directories; the doc comment on `impl OwnershipStore` still says "background compaction off … leave it off" and "Only the WAL is collected. Manifest and compacted objects stay untouched". Both are false and the next reader will believe one of them. The `// With compaction on ...` line at :261-263 is the truth and is stranded between two doc comments. The doc line "Every entry currently in the map, for pruning and for `/healthz` diagnostics" (:388) sits on `set_draining`; it belongs on `all()` (:427).

- [ ] **Step 1: Edit the comments**

Delete :227-231 (the "compaction off" paragraph) and :243-246 (the "Only the WAL is collected" paragraph). Turn :261-263 into the lead paragraph of the same doc comment:

```rust
    /// Leader: opens for writing with compaction ON and every collector at its default — see
    /// `leader_settings` for why that is safe for a `FollowLatest` follower. With those, the
    /// map's object count is bounded: steady state is the live SSTs plus at most fifteen minutes
    /// of compaction orphans (the compactor's checkpoint lifetime) and one collector interval.
    ///
    /// Follower: opens read-only, polling the manifest so its view of the map catches up on its
    /// own schedule rather than the request path.
    ///
    /// WAL garbage collection ...   (keep :236-241 and :248-260 as they are)
```

Move `/// Every entry currently in the map, for pruning and for \`/healthz\` diagnostics.` from above `set_draining` to above `pub async fn all(`.

- [ ] **Step 2: Verify it builds and the ownership tests pass**

Run: `cargo test --lib ownership`
Expected: PASS (comments only).

- [ ] **Step 3: Commit**

```bash
git add src/ownership.rs
git commit -m "Drop the ownership-map comments that compaction made false"
```

---

### Task 15: An unparseable pack-index value re-lists instead of re-downloading forever

**Files:**
- Modify: `src/store.rs:275-303` (`pack_index`)
- Test: `tests/store.rs`

**Context:** `parse().unwrap_or(0)` turns a corrupt index row into size 0, so `fetch_pack_file`'s size-equality skip never matches and the pack is downloaded on every open. A bad row means the index is not trustworthy; fall through to the listing, which re-records every file.

- [ ] **Step 1: Write the failing test**

```rust
/// A corrupt index row must not mean "size 0, re-download on every open" — it means the index
/// is untrustworthy, so the listing fallback runs and repairs it.
#[tokio::test]
async fn a_corrupt_pack_index_row_falls_back_to_the_listing() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("a", "r").await.unwrap();
    let repo = s.open_repo("a", "r").await.unwrap().unwrap();
    let p = repo.pack_dir.join("pack-abc.pack");
    let i = repo.pack_dir.join("pack-abc.idx");
    std::fs::write(&p, b"PACKDATA").unwrap();
    std::fs::write(&i, b"IDX").unwrap();
    s.upload_pack_files(&repo, &p, &i).await.unwrap();
    s.db_for("a", "r").await.unwrap().put(b"pack/a/r/pack-abc.pack", b"junk").await.unwrap();

    let files = s.pack_index("a", "r").await.unwrap();
    let pack = files.iter().find(|(f, _)| f == "pack-abc.pack").unwrap();
    assert_eq!(pack.1, 8, "size came from the listing, not the corrupt row: {files:?}");
    let repaired = s.db_for("a", "r").await.unwrap().get(b"pack/a/r/pack-abc.pack").await.unwrap().unwrap();
    assert_eq!(&repaired[..], b"8", "and the row was rewritten");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test store a_corrupt_pack_index_row_falls_back_to_the_listing`
Expected: FAIL — size is 0.

- [ ] **Step 3: Fall through on a bad row**

Replace the scan loop in `pack_index`:

```rust
        let mut out = Vec::new();
        while let Some(kv) = it.next().await? {
            let fname = String::from_utf8_lossy(&kv.key[prefix.len()..]).to_string();
            // One bad row makes the whole index suspect: fall through to the listing, which
            // re-records every file. Defaulting the size to 0 instead meant the size-equality
            // skip in `fetch_pack_file` never matched, and the pack was downloaded on every open.
            let Ok(size) = String::from_utf8_lossy(&kv.value).parse::<u64>() else {
                out.clear();
                break;
            };
            out.push((fname, size));
        }
```

Update the doc comment's "Falls back to listing the object store when the index is empty" to "when the index is empty or unreadable".

- [ ] **Step 4: Run tests**

Run: `cargo test --test store`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs tests/store.rs
git commit -m "Re-list packs when an index row is unreadable"
```

---

### Task 16: One malformed commit header does not 500 the log

**Files:**
- Modify: `src/browse.rs:186-203` (`log`)
- Test: `tests/browse.rs`

**Context:** `c.author()?` and `c.time()?` fail the whole page for a commit whose author line git accepted but gix's stricter parser does not. Display fields degrade; the walk continues.

- [ ] **Step 1: Write the failing test**

```rust
/// A commit whose author line gix cannot parse (git accepted it) shows blank fields; it must not
/// take the whole log page down with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_author_line_degrades_one_row_not_the_page() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "odd").await;
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    // A hand-built commit on top of HEAD with a time gix cannot parse.
    let raw = format!(
        "tree {}\nparent {head}\nauthor t <t@t> notatime +0000\ncommitter t <t@t> notatime +0000\n\nodd\n",
        common_tree_of(&repo, head)
    );
    let mut staging = kloudlite::objects::Staging::default();
    let odd = staging.add(gix_object::Kind::Commit, raw.into_bytes()).unwrap();
    staging.write(&e.store, &repo).await.unwrap();

    let odb = repo.odb().unwrap();
    let log = browse::log(&odb, odd, 10).unwrap();
    assert_eq!(log.len(), 3, "the odd commit and both fixture commits");
    assert_eq!(log[0].author, "", "unparseable author reads blank");
    assert_eq!(log[0].time, 0);
    assert_eq!(log[1].oid, head.to_hex().to_string());
}

fn common_tree_of(repo: &kloudlite::store::Repo, oid: gix_hash::ObjectId) -> String {
    let odb = repo.odb().unwrap();
    gix_object::FindExt::find_commit(&odb, &oid, &mut Vec::new()).unwrap().tree().to_hex().to_string()
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test browse a_malformed_author_line_degrades_one_row_not_the_page`
Expected: FAIL — `browse::log` returns `Err` on `c.time()?`. If instead `find_commit` itself refuses to decode the commit (the time field is kept raw by gix-actor, so decoding should succeed — but confirm), then the review finding does not apply and this task is dropped with that note in the commit body of the next task.

- [ ] **Step 3: Degrade the fields**

```rust
        out.push(Commit {
            oid: id.to_hex().to_string(),
            parents: c.parents().map(|p| p.to_hex().to_string()).collect(),
            // Display fields, not structure: an author line git accepted but gix cannot parse
            // reads blank rather than failing the page for every commit around it.
            author: c.author().map(|a| a.name.to_string()).unwrap_or_default(),
            time: c.time().map(|t| t.seconds).unwrap_or_default(),
            message: c.message.to_string(),
        });
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test browse`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/browse.rs tests/browse.rs
git commit -m "Show blank author fields for a commit gix cannot parse"
```

---

### Task 17: `write_err` cannot emit a corrupt pkt-line

**Files:**
- Modify: `src/pktline.rs:110-117` (`write_err`)
- Test: `src/pktline.rs` inline tests

**Context:** `write_pkt` refuses a payload over `0xffff - 4`; `write_err` formats the length by hand with no check, so a long error message goes out with a wrapped length the client cannot parse. Truncate on a char boundary.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn write_err_truncates_rather_than_corrupting_the_length() {
        let mut out: Vec<u8> = Vec::new();
        write_err(&mut out, &"é".repeat(60_000)).await.unwrap();
        let mut c = Cursor::new(out);
        let Some(Pkt::Data(d)) = read_pkt(&mut c).unwrap() else { panic!("not a data pkt") };
        assert!(d.starts_with(b"ERR "));
        assert!(d.len() + 4 <= 0xffff);
        assert!(std::str::from_utf8(&d).is_ok(), "truncated on a char boundary");
        assert!(read_pkt(&mut c).unwrap().is_none(), "one pkt, nothing trailing");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib pktline::tests::write_err_truncates`
Expected: FAIL — `read_pkt` reads a wrapped length and the assertions fall over (or the 120 KB body is not one pkt).

- [ ] **Step 3: Bound the message**

```rust
pub async fn write_err<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, msg: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    // Bounded like `write_pkt`: a pkt-line is at most 0xffff bytes, and an ERR that does not fit
    // would go out with a wrapped length the client cannot parse. Long messages are cut, not
    // refused — a refusal that cannot be delivered is no refusal.
    const MAX_MSG: usize = 0xffff - 4 - "ERR \n".len();
    let mut end = msg.len().min(MAX_MSG);
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    let body = format!("ERR {}\n", &msg[..end]);
    w.write_all(format!("{:04x}{body}", body.len() + 4).as_bytes()).await?;
    w.flush().await
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib pktline`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/pktline.rs
git commit -m "Truncate long ERR pkt-lines instead of corrupting the length"
```

---

### Task 18: A ref name needs a component after its namespace

**Files:**
- Modify: `src/protocol/receive.rs:183-204` (`valid_ref_name`)
- Test: `src/protocol/receive.rs` inline tests (new module)

**Context:** `refs/heads` and `refs/tags` pass `valid_ref_name`; a ref stored under either collides with the namespace every listing and protection check assumes is a directory. git itself accepts `refs/heads` as a name, which is why this is our rule, not git's: require `refs/<namespace>/<something>`. Also the table test the review asked for.

- [ ] **Step 1: Write the failing table test**

```rust
#[cfg(test)]
mod ref_name_tests {
    use super::valid_ref_name;

    #[test]
    fn valid_ref_name_table() {
        for ok in ["refs/heads/main", "refs/tags/v1.0", "refs/heads/feature/x-y_z", "refs/notes/commits"] {
            assert!(valid_ref_name(ok), "{ok} should be accepted");
        }
        for bad in [
            "refs/heads",          // the namespace itself; a ref here shadows every branch
            "refs/tags",
            "refs",
            "refs/",
            "refs/heads/",
            "heads/main",          // not under refs/
            "refs/heads/.hidden",
            "refs/heads/a..b",
            "refs/heads/a.lock",
            "refs/heads/a b",
            "refs/heads/a~b",
            "refs/heads/a^b",
            "refs/heads/a:b",
            "refs/heads/a?b",
            "refs/heads/a*b",
            "refs/heads/a[b",
            "refs/heads/a\\b",
            "refs/heads/a@{b",
            "refs/heads//x",
            "refs/heads/a\x7fb",
            "refs/heads/a\nb",
        ] {
            assert!(!valid_ref_name(bad), "{bad:?} should be refused");
        }
        assert!(!valid_ref_name(&format!("refs/heads/{}", "a".repeat(600))), "too long");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib receive::ref_name_tests`
Expected: FAIL on `refs/heads` and `refs/tags`.

- [ ] **Step 3: Require the third component**

In `valid_ref_name`, extend the first condition:

```rust
    if !name.starts_with("refs/")
        || name.len() > 512
        || name.ends_with('/')
        || name.ends_with(".lock")
        // `refs/heads` is a legal git name and an illegal one here: listings and protection
        // rules treat each namespace as a directory, and a ref AT the namespace shadows them.
        || name.splitn(3, '/').count() < 3
    {
        return false;
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib receive` then `cargo test --test protocol`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/protocol/receive.rs
git commit -m "Require a component after the ref namespace"
```

---

### Task 19: Only a trailing `*` in a protection pattern

**Files:**
- Modify: `src/refs.rs:391-405` (`set_protection`)
- Test: `tests/store.rs`

**Context:** `Protection::matches` only honours a TRAILING `*`; `set_protection` accepts `rel*ease`, which then matches nothing and the person thinks the branch is protected. Refuse it where the rule is created.

- [ ] **Step 1: Write the failing test**

```rust
/// `matches` only understands a trailing `*`; a pattern with one anywhere else would be stored,
/// match nothing, and leave its author believing the branch is protected.
#[tokio::test]
async fn a_non_trailing_star_is_refused() {
    let e = common::env().await;
    let s = &e.store;
    s.create_repo("alice", "web").await.unwrap();
    let p = |pattern: &str| kloudlite::refs::Protection { pattern: pattern.into(), no_force: true, no_delete: true };
    assert!(s.set_protection("alice", "web", &p("rel*ease")).await.is_err());
    assert!(s.set_protection("alice", "web", &p("*/main")).await.is_err());
    assert!(s.set_protection("alice", "web", &p("release/*")).await.is_ok());
    assert!(s.set_protection("alice", "web", &p("main")).await.is_ok());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test store a_non_trailing_star_is_refused`
Expected: FAIL — `rel*ease` is accepted.

- [ ] **Step 3: Refuse it**

In `set_protection`, after the `//` check:

```rust
        // `matches` honours a trailing `*` and nothing else; a pattern with one elsewhere would
        // be stored, match nothing, and read as protection to whoever wrote it.
        let stem = p.pattern.strip_suffix('*').unwrap_or(&p.pattern);
        if stem.contains('*') {
            return Err(err("only a trailing * is supported in a branch pattern"));
        }
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test store`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/refs.rs tests/store.rs
git commit -m "Refuse protection patterns with a non-trailing star"
```

---

### Task 20: Judge a signature at the time it was made

**Files:**
- Modify: `src/gpg.rs:136-183` (`verify`)
- Test: `src/gpg.rs` inline tests

**Context:** `verify` checks the key's validity NOW and never looks at the signature's own timestamps. Three holes, all on `Signature` in pgp 0.20 (`~/.cargo/registry/src/*/pgp-0.20.0/src/packet/signature/types.rs:843-863`): `created() -> Option<Timestamp>`, `key_expiration_time()`, `signature_expiration_time() -> Option<Duration>`. A signature dated before its key existed is forged or misattributed; one dated past the key's expiry was made with a retired key; one carrying its own expiry that has passed says, in the signer's words, not to trust it.

- [ ] **Step 1: Write the failing tests**

In `mod tests`:

```rust
    #[test]
    fn a_signature_that_predates_its_key_is_invalid() {
        // The key comes into existence tomorrow; the signature is made now.
        let sk = gen("l@example.com", SystemTime::now() + Duration::from_secs(86_400));
        let pk: SignedPublicKey = sk.clone().into();
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let payload = b"commit body";
        let sig = subkey_signature(&sk, payload);
        assert_eq!(verify(&armored, &sig, payload, "l@example.com"), Reason::Invalid);
    }

    #[test]
    fn a_signature_past_its_own_expiry_is_invalid() {
        use pgp::composed::SubpacketConfig;
        let sk = gen("n@example.com", SystemTime::now() - Duration::from_secs(30 * 86_400));
        let pk: SignedPublicKey = sk.clone().into();
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let payload = b"commit body";
        let signer = &sk.secret_subkeys[0].key;
        // Made two days ago, valid for one.
        let hashed = vec![
            Subpacket::regular(SubpacketData::SignatureCreationTime(
                Timestamp::try_from(SystemTime::now() - Duration::from_secs(2 * 86_400)).unwrap(),
            ))
            .unwrap(),
            Subpacket::regular(SubpacketData::IssuerFingerprint(signer.fingerprint())).unwrap(),
            Subpacket::regular(SubpacketData::SignatureExpirationTime(PgpDuration::from_secs(86_400))).unwrap(),
        ];
        let sig = DetachedSignature::sign_binary_data_with_subpackets(
            rand::thread_rng(),
            signer,
            &Password::empty(),
            HashAlgorithm::Sha256,
            &payload[..],
            SubpacketConfig::UserDefined { hashed, unhashed: vec![] },
        )
        .unwrap()
        .to_armored_string(ArmorOptions::default())
        .unwrap();
        assert_eq!(verify(&armored, &sig, payload, "n@example.com"), Reason::Invalid);
    }

    #[test]
    fn a_signature_made_after_the_key_expired_is_expired_key() {
        // Key created 2y ago with a 1y expiry (so expired now); the signature is dated 18
        // months ago — inside "expired" territory even though nobody has moved the clock.
        let two_years = Duration::from_secs(2 * 365 * 86_400);
        let mut sk = gen("o@example.com", SystemTime::now() - two_years);
        let mut cfg = SignatureConfig::from_key(rand::thread_rng(), &sk.primary_key, SignatureType::Key).unwrap();
        cfg.hashed_subpackets = vec![
            Subpacket::regular(SubpacketData::SignatureCreationTime(
                Timestamp::try_from(SystemTime::now() - two_years).unwrap(),
            ))
            .unwrap(),
            Subpacket::regular(SubpacketData::IssuerFingerprint(sk.primary_key.fingerprint())).unwrap(),
            Subpacket::regular(SubpacketData::KeyExpirationTime(PgpDuration::from_secs(365 * 86_400))).unwrap(),
        ];
        let direct = cfg.sign_key(&sk.primary_key, &Password::empty(), &sk.primary_key.public_key()).unwrap();
        sk.details.direct_signatures.push(direct);
        let pk: SignedPublicKey = sk.clone().into();
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let sig = subkey_signature(&sk, b"commit body");
        assert_eq!(verify(&armored, &sig, b"commit body", "o@example.com"), Reason::ExpiredKey);
    }
```

The third test is satisfied by the existing NOW-based `validity` check as well; it stays as the guard that the made-at check does not change that answer.

- [ ] **Step 2: Run tests to verify the first two fail**

Run: `cargo test --lib gpg::tests`
Expected: `a_signature_that_predates_its_key_is_invalid` and `a_signature_past_its_own_expiry_is_invalid` FAIL with `Valid`.

- [ ] **Step 3: Add the made-at checks**

In `verify`, after the `validity` match and before `let ok = ...`:

```rust
    // Judged at the moment the signature was MADE, not only now. One dated before its key
    // existed can only be forged or misattributed; one dated past the key's expiry was made with
    // a retired key however the clock reads today; one carrying its own expiry that has passed
    // says, in the signer's words, not to trust it any more.
    use pgp::types::KeyDetails;
    let key_created: std::time::SystemTime = key.primary_key.created_at().into();
    let Some(made) = sig.signature.created() else {
        return Reason::Invalid;
    };
    let made: std::time::SystemTime = made.into();
    if made < key_created {
        return Reason::Invalid;
    }
    if let Some(d) = effective_expiry(&key) {
        if key_created + std::time::Duration::from(d) < made {
            return Reason::ExpiredKey;
        }
    }
    if let Some(d) = sig.signature.signature_expiration_time() {
        if made + std::time::Duration::from(d) < now {
            return Reason::Invalid;
        }
    }
```

Update the doc comment on `verify` ("Expiry is judged BEFORE the maths") to mention the signature's own timestamps are checked there too.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib gpg` then `cargo test --test api_server`
Expected: PASS — every pre-existing `verify` test signs "now" with a key created "now" or earlier.

- [ ] **Step 5: Commit**

```bash
git add src/gpg.rs
git commit -m "Check a signature's timestamps against its key and its own expiry"
```

---

### Task 21: Use the crate-wide hex encoder in `gpg.rs`

**Files:**
- Modify: `src/gpg.rs:76-78` (delete `fn hex`), callers at :70, :72, :91, :95 and the tests at :391, :424, :557
- Depends on: the registry plan's task that adds `pub(crate) fn hex(b: &[u8]) -> String` to `src/lib.rs`. **Do this task after that lands.** If it has not and this task is reached, add the one-liner yourself — the registry plan's task will then find it present and skip:

```rust
/// Lowercase hex. One definition: four copies of this loop were in the tree.
pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
```

- [ ] **Step 1: Replace the local definition**

Delete `fn hex` from `gpg.rs` and add `use crate::hex;` at the top. Every call site (`hex(f.as_bytes())`, `hex(k.as_ref())`, `hex(s.key.fingerprint().as_bytes())`, and the three in tests) compiles unchanged.

- [ ] **Step 2: Run tests**

Run: `cargo test --lib gpg`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/gpg.rs src/lib.rs
git commit -m "Use the shared hex encoder in gpg"
```

---

### Task 22: One `peel_wants`

**Files:**
- Modify: `src/protocol/upload.rs` — the three peel loops in `reachable_set_hiding` (:663-684), `commit_range` (:707-730) and `write_pack` (Task 1's shape)

**Context:** The same "follow tags to a commit; collect commits for walking and everything else for sending as-is" loop exists three times. After Task 1, `write_pack` needs tags separate from trees/blobs; the other two callers concatenate them.

**Interfaces:**
- Produces: `struct Peeled { commits: Vec<ObjectId>, tags: Vec<ObjectId>, leaves: Vec<ObjectId> }` and `fn peel_wants(odb: &gix_odb::Handle, wants: &[ObjectId]) -> Result<Peeled>`.

- [ ] **Step 1: Add the helper and use it three times**

```rust
/// What a list of wants splits into: commits (walkable), the tags passed through on the way to
/// them (sent as-is), and trees or blobs wanted directly (a promisor fetch; sent as-is).
struct Peeled {
    commits: Vec<ObjectId>,
    tags: Vec<ObjectId>,
    leaves: Vec<ObjectId>,
}

fn peel_wants(odb: &gix_odb::Handle, wants: &[ObjectId]) -> Result<Peeled> {
    let mut buf = Vec::new();
    let mut p = Peeled { commits: Vec::new(), tags: Vec::new(), leaves: Vec::new() };
    for w in wants {
        let mut id = *w;
        loop {
            match gix_object::FindExt::find(odb, &id, &mut buf)?.decode()? {
                gix_object::ObjectRef::Commit(_) => {
                    p.commits.push(id);
                    break;
                }
                gix_object::ObjectRef::Tag(t) => {
                    p.tags.push(id);
                    id = t.target();
                }
                _ => {
                    p.leaves.push(id);
                    break;
                }
            }
        }
    }
    Ok(p)
}
```

`reachable_set_hiding`:
```rust
    let Peeled { commits, tags, leaves } = peel_wants(odb, &tips)?;
    let mut ids = tags;
    ids.extend(leaves);
    for info in gix_traverse::commit::Simple::new(commits, odb.clone()).hide(hide)? {
        ids.push(info?.id);
    }
```

`commit_range`:
```rust
    let Peeled { commits, tags, leaves } = peel_wants(odb, &wants)?;
    let mut ids = tags;
    ids.extend(leaves);
    for info in gix_traverse::commit::Simple::new(commits, odb.clone()).hide(haves)? {
        ids.push(info?.id);
    }
    Ok(ids)
```

`write_pack`: replace its inline loop with `let Peeled { commits: tips, tags, leaves } = peel_wants(odb, &wants)?;` and keep the rest from Task 1. Keep the "Only commits can be walked..." comment on the helper, not at each call.

- [ ] **Step 2: Run tests**

Run: `cargo test --test protocol` then `cargo test --test http_e2e`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add src/protocol/upload.rs
git commit -m "Peel wants in one place"
```

---

### Task 23: Fix the three stale comments

**Files:**
- Modify: `src/protocol/receive.rs:240-248`, `src/cache.rs:323-326`, `src/protocol/mod.rs:6-9`

- [ ] **Step 1: `receive.rs:246` — forks no longer share a pool**

Replace the paragraph starting `// Two things fall out of this.` with:

```rust
    // Two things fall out of this. A pack with holes fails the walk (the missing object can't be
    // read) instead of creating a ref whose history is broken. And "exists in the local odb" is
    // NOT accepted, because the cache can hold objects this repo does not own: a pack from a push
    // that was rejected after indexing, or a pack a repack elsewhere has since dropped and the
    // prune has not yet reached.
```

- [ ] **Step 2: `cache.rs:326` — there is no Mongo on the merge path**

Replace `since \`check_one\` and \`claim_merge\` are themselves idempotent claims against Mongo.` with `since \`check_one\` and \`claim_merge\` are themselves idempotent claims in the repo's own database (\`pulls::claim_merge\`).`

- [ ] **Step 3: `protocol/mod.rs:7` — say what `block_in_place` demands**

```rust
/// Run a future to completion from sync code inside `spawn_blocking`.
///
/// `block_in_place` turns the CURRENT worker thread into a blocking one, so this must only run
/// on a multi-thread runtime (`flavor = "multi_thread"` in every test that reaches it) and never
/// from a `LocalSet`; on a current-thread runtime it panics.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
```

- [ ] **Step 4: Verify and commit**

Run: `cargo build`
Expected: builds.

```bash
git add src/protocol/receive.rs src/cache.rs src/protocol/mod.rs
git commit -m "Correct three comments the code has moved past"
```

---

### Task 24: Release profile and the dependency-duplication note

**Files:**
- Modify: `Cargo.toml`

**Context:** No `[profile.release]`: the shipped binary is built without LTO, with 16 codegen units, and with symbols. `thin` LTO is the cheap win (`fat` roughly doubles the CI build for a few percent more). The review's dependency note (644 crates, `rand` 0.8/0.9/0.10, `rsa` twice incl. `0.10.0-rc` via `russh`/`ssh-key`) is recorded as a comment where the next person adding a dependency will read it; no code change, because every duplicate is pinned by an upstream crate this project does not control.

- [ ] **Step 1: Add the profile and the note**

Append to `Cargo.toml`:

```toml
# Measured on nothing yet: thin LTO and one codegen unit are the conventional cheap wins for a
# binary that is built once per commit and runs for days; `strip` keeps the image small. Switch
# to `lto = "fat"` only with a benchmark in hand — it roughly doubles the link time for a few
# percent.
[profile.release]
lto = "thin"
codegen-units = 1
strip = true
```

Above the `rand = "0.8"` line in `[dependencies]`:

```toml
# Three `rand` versions (0.8 here, 0.9 and 0.10 under russh/ssh-key/pgp) and two `rsa` (one a
# release candidate, pulled by russh's ssh-key) are in the lock. Each is pinned by an upstream
# crate; bumping this one alone would not collapse them. Revisit when russh and pgp agree on a
# rand line — `cargo tree -d` is the check.
```

- [ ] **Step 2: Verify a release build links**

Run: `cargo build --release --bin kloudlite`
Expected: links clean (takes longer than a debug build).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml
git commit -m "Add a release profile and note the rand/rsa duplication"
```

---

## Final verification (after all tasks)

- [ ] `cargo test` — full suite green.
- [ ] `cargo clippy --lib` — no NEW warnings in `src/protocol/*`, `src/store.rs`, `src/objects.rs`, `src/refs.rs`, `src/pktline.rs`, `src/pool.rs`, `src/cache.rs`, `src/ownership.rs`, `src/browse.rs`, `src/gpg.rs`.
- [ ] `./tests/registry_e2e.sh` if docker is up (exit 77 means the docker half was skipped).
- [ ] Re-read `docs/code-review-2026-08-23.md` sections 0, 2, 3, 4, 5 and confirm every git-core row maps to a landed commit or to the exclusions listed at the top of this plan.

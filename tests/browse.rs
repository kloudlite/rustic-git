
mod common;
use rustic_git::browse;

#[tokio::test(flavor = "multi_thread")]
async fn reads_a_tree_a_blob_and_a_diff() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "web").await; // two commits; src/main.rs changes
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();

    let root = browse::tree_at(&odb, head, "").unwrap();
    assert!(root.iter().any(|x| x.name == "src" && x.kind == "tree"));

    let sub = browse::tree_at(&odb, head, "src").unwrap();
    assert!(sub.iter().any(|x| x.name == "main.rs" && x.kind == "blob"));

    let blob = browse::blob_at(&odb, head, "src/main.rs", 5 << 20).unwrap();
    assert!(!blob.truncated);
    assert!(String::from_utf8_lossy(&blob.bytes).contains("fn main"));

    let truncated = browse::blob_at(&odb, head, "src/main.rs", 4).unwrap();
    assert!(truncated.truncated && truncated.bytes.len() == 4);

    let commits = browse::log(&odb, head, 10).unwrap();
    assert_eq!(commits.len(), 2, "fixture has two commits");
    assert_eq!(commits[0].oid, head.to_hex().to_string());

    let json = serde_json::to_value(&blob).unwrap();
    assert!(json["bytes_base64"].is_string(), "blob bytes travel as base64: {json}");

    let (c, diff) = browse::commit(&odb, head).unwrap();
    assert_eq!(c.parents.len(), 1);
    assert!(diff.contains("src/main.rs"), "diff names the changed file: {diff}");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_path_is_an_error_not_a_panic() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "web").await;
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    assert!(browse::tree_at(&odb, head, "nope").is_err());
    assert!(browse::blob_at(&odb, head, "src", 1024).is_err(), "a tree is not a blob");
}

/// Every shape the hand-rolled tree walk has to get right: add, delete, a nested directory
/// deleted wholesale, a file replaced by a directory and back, and the root commit.
#[tokio::test(flavor = "multi_thread")]
async fn diff_covers_adds_deletes_and_type_swaps() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "shapes", |c| {
        std::fs::create_dir_all(c.join("dir/sub")).unwrap();
        std::fs::write(c.join("README.md"), "hello\n").unwrap();
        std::fs::write(c.join("dir/a.txt"), "a\n").unwrap();
        std::fs::write(c.join("dir/sub/b.txt"), "b\n").unwrap();
        std::fs::write(c.join("thing"), "a file\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "root"]);

        // add + delete + directory deleted + file replaced by a directory
        std::fs::write(c.join("new.txt"), "new\n").unwrap();
        std::fs::remove_file(c.join("README.md")).unwrap();
        std::fs::remove_dir_all(c.join("dir")).unwrap();
        std::fs::remove_file(c.join("thing")).unwrap();
        std::fs::create_dir(c.join("thing")).unwrap();
        std::fs::write(c.join("thing/x.txt"), "x\n").unwrap();
        common::git(c, &["add", "-A"]);
        common::git(c, &["commit", "-qm", "shapes"]);

        // and the mirror: a directory replaced by a file
        std::fs::remove_dir_all(c.join("thing")).unwrap();
        std::fs::write(c.join("thing"), "a file again\n").unwrap();
        common::git(c, &["add", "-A"]);
        common::git(c, &["commit", "-qm", "swap back"]);
    })
    .await;
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let history = browse::log(&odb, head, 10).unwrap();
    assert_eq!(history.len(), 3);
    let oid = |i: usize| history[i].oid.parse().unwrap();

    // Directory -> file: the whole old subtree is deleted and the new file added.
    let (_, swap_back) = browse::commit(&odb, oid(0)).unwrap();
    assert!(swap_back.contains("thing/x.txt"), "deleted subtree: {swap_back}");
    assert!(swap_back.contains("+a file again"), "added file: {swap_back}");

    let (c, d) = browse::commit(&odb, oid(1)).unwrap();
    assert_eq!(c.parents.len(), 1);
    for want in ["new.txt", "README.md", "dir/a.txt", "dir/sub/b.txt", "thing", "thing/x.txt"] {
        assert!(d.contains(want), "{want} missing from diff: {d}");
    }
    assert!(d.contains("+new\n"), "the addition's content: {d}");
    assert!(d.contains("-b\n"), "the nested deletion's content: {d}");
    // File -> directory: the old blob's deletion is not lost.
    assert!(d.contains("-a file\n"), "the replaced file's deletion: {d}");
    assert!(d.contains("+x\n"), "the new subtree's content: {d}");

    // Root commit: everything is an addition, diffed against no parent at all.
    let (root, d) = browse::commit(&odb, oid(2)).unwrap();
    assert!(root.parents.is_empty());
    for want in ["README.md", "dir/sub/b.txt", "+hello", "+a file"] {
        assert!(d.contains(want), "{want} missing from root diff: {d}");
    }
}

/// Sizes come from each object's header rather than from inflating it — 73x
/// faster on a directory holding a large file. The risk that buys is a size that
/// is not the size, so it is checked against the bytes themselves.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_reports_the_size_the_bytes_actually_are() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_fixture(&e, "alice", "web").await;
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();

    let mut checked = 0;
    for entry in browse::tree_at(&odb, head, "src").unwrap() {
        if entry.kind != "blob" { continue; }
        let bytes = browse::blob_at(&odb, head, &format!("src/{}", entry.name), 1 << 30).unwrap();
        assert_eq!(entry.size, Some(bytes.bytes.len() as u64), "{}", entry.name);
        checked += 1;
    }
    assert!(checked > 0, "the fixture should have a blob to check");

    // The whole-tree walk carries sizes too — the language breakdown is byte
    // counts, so an absent size there silently drops a file from the totals.
    let files = browse::files_at(&odb, head, "", 5000).unwrap();
    assert!(files.iter().any(|f| f.name == "src/main.rs"), "paths are full, not just names");
    assert!(files.iter().all(|f| f.size.is_some()), "every file needs a size");
}

/// A binary file is named in the diff and its contents are not.
///
/// Diffing bytes as lossy UTF-8 produced pages of replacement characters — a
/// favicon rendered as 31 lines of mojibake, burying every real hunk in the
/// commit. Detection follows git's rule: a NUL byte near the start.
#[tokio::test(flavor = "multi_thread")]
async fn a_binary_file_is_named_but_not_rendered() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "bin", |c| {
        std::fs::write(c.join("readme.md"), "before\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "one"]);
        // A PNG header: a NUL in the first bytes is exactly what git looks for.
        std::fs::write(c.join("logo.png"), [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x00, 0x1A]).unwrap();
        std::fs::write(c.join("readme.md"), "after\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "two"]);
    })
    .await;
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();

    let (_, diff) = browse::commit(&odb, head).unwrap();

    assert!(diff.contains("+++ b/logo.png"), "the binary file is still named");
    assert!(diff.contains(browse::BINARY_MARKER), "and marked as binary");
    assert!(!diff.contains('\u{FFFD}'), "no replacement characters anywhere in the diff");
    // The text file beside it still diffs normally — detection must be per file.
    assert!(diff.contains("+after"), "a text file in the same commit still diffs");
}

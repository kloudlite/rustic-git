
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

/// A comparison is taken from the MERGE BASE, not from the base tip — otherwise
/// commits that landed on the base since the branch left it are attributed to the
/// person proposing the change.
#[tokio::test(flavor = "multi_thread")]
async fn a_comparison_shows_only_what_the_branch_added() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "pr", |c| {
        std::fs::write(c.join("shared.txt"), "base\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "root"]);

        // A branch that adds one file...
        common::git(c, &["checkout", "-qb", "feature"]);
        std::fs::write(c.join("mine.txt"), "mine\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "add mine"]);

        // ...while the base moves on independently.
        common::git(c, &["checkout", "-q", "master"]);
        std::fs::write(c.join("theirs.txt"), "theirs\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "add theirs"]);
        common::git(c, &["checkout", "-q", "feature"]);
    })
    .await;
    let odb = repo.odb().unwrap();
    // push_built pushes HEAD (feature) to master, so both refs exist under names
    // we can resolve through the odb by walking; take the tips from the refs.
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();

    // Comparing a commit with itself is empty, and is trivially a fast-forward.
    let same = browse::compare(&odb, head, head, 50).unwrap();
    assert!(same.commits.is_empty() && same.diff.is_empty());
    assert!(same.fast_forward, "nothing to do is a fast-forward");
    assert_eq!(same.merge_base.as_deref(), Some(head.to_hex().to_string().as_str()));

    // A parent to its child: one commit, and the base IS the merge base.
    let parent: gix_hash::ObjectId = browse::log(&odb, head, 2).unwrap()[1].oid.parse().unwrap();
    let ahead = browse::compare(&odb, parent, head, 50).unwrap();
    assert_eq!(ahead.commits.len(), 1, "only the commit the branch added");
    assert!(ahead.fast_forward, "the base is an ancestor, so it can move");
    assert_eq!(ahead.merge_base.as_deref(), Some(parent.to_hex().to_string().as_str()));
    assert!(ahead.diff.contains("+++ b/"), "and it carries a diff");

    // The other direction is not a fast-forward: the base is ahead.
    let behind = browse::compare(&odb, head, parent, 50).unwrap();
    assert!(!behind.fast_forward, "moving backwards is not a fast-forward");
    assert!(behind.commits.is_empty(), "the older commit adds nothing");
}

/// A signed commit yields the signature and exactly the bytes it covers.
///
/// The payload is the part that goes wrong quietly: git signs the commit object
/// with the `gpgsig` header removed, and a signature spans continuation lines, so
/// trimming it by hand is how every signature ends up "invalid" for no visible
/// reason. `ssh-keygen -Y verify` is the oracle here — if it accepts what we
/// rebuilt, the bytes are right.
#[tokio::test(flavor = "multi_thread")]
async fn a_signed_commit_yields_the_signature_and_the_bytes_it_covers() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    let keydir = tempfile::tempdir().unwrap();
    let key = keydir.path().join("id");
    let gen = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "signer@example.com", "-f"])
        .arg(&key)
        .output();
    if !matches!(&gen, Ok(o) if o.status.success()) {
        eprintln!("skipping: no ssh-keygen");
        return;
    }

    let e = common::env().await;
    let repo = common::push_built(&e, "alice", "signed", |c| {
        let key = key.to_str().unwrap();
        common::git(c, &["config", "gpg.format", "ssh"]);
        common::git(c, &["config", "user.signingkey", key]);
        // The harness forces GIT_AUTHOR_EMAIL, and the environment beats config —
        // so the commit's author is the harness's identity, not this one.
        std::fs::write(c.join("f.txt"), "one\n").unwrap();
        common::git(c, &["add", "."]);
        common::git(c, &["commit", "-qm", "signed commit", "-S"]);
    })
    .await;
    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();

    let signed = browse::signature_of(&odb, head).unwrap().expect("the commit is signed");
    assert!(signed.signature.contains("BEGIN SSH SIGNATURE"), "an ssh signature");
    assert_eq!(signed.author_email, "t@t", "the author comes from the commit itself");
    assert!(!signed.payload.is_empty());
    assert!(
        !String::from_utf8_lossy(&signed.payload).contains("gpgsig"),
        "the payload is the commit WITHOUT its signature header",
    );

    // ssh-keygen decides, not us: write an allowed-signers file and ask it.
    let dir = tempfile::tempdir().unwrap();
    let pubkey = std::fs::read_to_string(format!("{}.pub", key.display())).unwrap();
    let allowed = dir.path().join("allowed");
    std::fs::write(&allowed, format!("t@t {pubkey}")).unwrap();
    let sigfile = dir.path().join("sig");
    std::fs::write(&sigfile, &signed.signature).unwrap();
    let payload = dir.path().join("payload");
    std::fs::write(&payload, &signed.payload).unwrap();

    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ssh-keygen -Y verify -f {} -I t@t -n git -s {} < {}",
            allowed.display(), sigfile.display(), payload.display(),
        ))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ssh-keygen must accept the payload we rebuilt: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // An unsigned commit says so rather than erroring.
    let plain = common::push_fixture(&e, "alice", "plain").await;
    let podb = plain.odb().unwrap();
    let phead = e.store.get_ref(&plain, "refs/heads/master").await.unwrap().unwrap();
    assert!(browse::signature_of(&podb, phead).unwrap().is_none());
}

/// GPG verification, end to end: a real key, a real signed commit, and the same
/// payload reconstruction the ssh path uses.
///
/// The interesting parts are not the cryptography — they are the identity rules.
/// A commit is normally signed by a SUBKEY while the person is the primary key,
/// and a key may carry several emails. Getting either wrong turns a good
/// signature into `bad_email` for someone who did nothing wrong.
#[tokio::test(flavor = "multi_thread")]
async fn a_gpg_signed_commit_verifies_against_its_key() {
    if !common::have_git() { eprintln!("skipping: no git"); return; }
    if std::process::Command::new("gpg").arg("--version").output().is_err() {
        eprintln!("skipping: no gpg");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let gpg = |args: &[&str]| -> std::process::Output {
        std::process::Command::new("gpg")
            .env("GNUPGHOME", home.path())
            .args(args)
            .output()
            .unwrap()
    };
    // t@t is the identity the harness forces onto every commit.
    let made = gpg(&["--batch", "--passphrase", "", "--quick-generate-key", "t <t@t>", "ed25519", "sign", "never"]);
    if !made.status.success() {
        eprintln!("skipping: gpg cannot generate here: {}", String::from_utf8_lossy(&made.stderr));
        return;
    }
    let key_id = String::from_utf8_lossy(&gpg(&["--list-secret-keys", "--with-colons", "t@t"]).stdout)
        .lines()
        .find_map(|l| l.strip_prefix("fpr:").map(|r| r.trim_matches(':').to_string()))
        .expect("a fingerprint");
    let armoured = String::from_utf8_lossy(&gpg(&["--armor", "--export", "t@t"]).stdout).to_string();
    assert!(armoured.contains("BEGIN PGP PUBLIC KEY BLOCK"), "exported a key");

    let e = common::env().await;
    let home_path = home.path().to_path_buf();
    let repo = common::push_built(&e, "alice", "gpg", |c| {
        common::git(c, &["config", "gpg.format", "openpgp"]);
        common::git(c, &["config", "user.signingkey", &key_id]);
        common::git(c, &["config", "gpg.program", "gpg"]);
        std::fs::write(c.join("f.txt"), "one\n").unwrap();
        common::git(c, &["add", "."]);
        // The harness's git() cannot carry GNUPGHOME, so this one runs directly.
        let out = std::process::Command::new("git")
            .current_dir(c)
            .env("GNUPGHOME", &home_path)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .args(["commit", "-qm", "gpg signed", "-S"])
            .output()
            .unwrap();
        assert!(out.status.success(), "signing failed: {}", String::from_utf8_lossy(&out.stderr));
    })
    .await;

    let odb = repo.odb().unwrap();
    let head = e.store.get_ref(&repo, "refs/heads/master").await.unwrap().unwrap();
    let signed = browse::signature_of(&odb, head).unwrap().expect("signed");
    assert!(rustic_git::gpg::is_pgp(&signed.signature), "a pgp signature");

    // The signature names its issuer, and the key answers to that name.
    let issuers = rustic_git::gpg::issuers(&signed.signature).unwrap();
    let known = rustic_git::gpg::fingerprints_of(&armoured).unwrap();
    assert!(
        issuers.iter().any(|i| known.iter().any(|k| k.ends_with(i) || i.ends_with(k))),
        "the issuer resolves to this key: issuers={issuers:?} known={known:?}",
    );
    assert!(rustic_git::gpg::emails_of(&armoured).unwrap().contains(&"t@t".to_string()));

    // The whole judgement.
    let reason = rustic_git::gpg::verify(&armoured, &signed.signature, &signed.payload, "t@t");
    assert_eq!(reason, rustic_git::gpg::Reason::Valid, "a good signature by the author's key");

    // Same signature, different author: good maths, wrong person.
    let reason = rustic_git::gpg::verify(&armoured, &signed.signature, &signed.payload, "someone@else");
    assert_eq!(reason, rustic_git::gpg::Reason::BadEmail);

    // Tampered payload.
    let mut altered = signed.payload.clone();
    altered.extend(b"\n");
    let reason = rustic_git::gpg::verify(&armoured, &signed.signature, &altered, "t@t");
    assert_eq!(reason, rustic_git::gpg::Reason::Invalid, "the bytes must match");
}

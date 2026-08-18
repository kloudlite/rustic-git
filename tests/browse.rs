
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

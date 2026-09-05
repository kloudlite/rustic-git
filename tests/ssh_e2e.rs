mod common;

#[tokio::test(flavor = "multi_thread")]
async fn ssh_clone_push() {
    if !common::have_git() || !common::have_ssh() {
        eprintln!("skip: git/ssh missing");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();

    let kd = tempfile::tempdir().unwrap();
    let key = kd.path().join("id_ed25519");
    assert!(std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let pubkey = std::fs::read_to_string(kd.path().join("id_ed25519.pub")).unwrap();
    let fp = common::ssh_fingerprint(&pubkey).unwrap();
    s.add_ssh_key("alice", &fp).await.unwrap();

    let host_key = gen_host_key(&kd);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let app = common::app(s.clone()).await;
    tokio::spawn(async move { kloudlite_vcs::ssh::serve(app, l, host_key).await.unwrap() });

    let ssh_cmd = format!(
        "ssh -i {} -p {port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        key.display()
    );
    let w = tempfile::tempdir().unwrap();
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            // Hermetic against the developer's own git config: `commit.gpgsign`
            // would send every fixture commit looking for a passphrase prompt.
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_SSH_COMMAND", &ssh_cmd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let url = format!("ssh://git@127.0.0.1:{port}/alice/proj.git");
    git(w.path(), &["clone", "-q", &url, "c1"]);
    let c1 = w.path().join("c1");
    std::fs::write(c1.join("f.txt"), "one\n").unwrap();
    git(&c1, &["add", "."]);
    git(&c1, &["commit", "-qm", "one"]);
    git(&c1, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    git(w.path(), &["clone", "-q", &url, "c2"]);
    assert_eq!(
        git(&w.path().join("c2"), &["rev-parse", "HEAD"]),
        git(&c1, &["rev-parse", "HEAD"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ssh_rejects_other_owner() {
    if !common::have_git()
        || std::process::Command::new("ssh")
            .arg("-V")
            .output()
            .is_err()
    {
        eprintln!("skip: git/ssh missing");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("bob", "proj").await.unwrap();
    let kd = tempfile::tempdir().unwrap();
    let key = kd.path().join("id_ed25519");
    assert!(std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let pubkey2 = std::fs::read_to_string(kd.path().join("id_ed25519.pub")).unwrap();
    let fp2 = common::ssh_fingerprint(&pubkey2).unwrap();
    s.add_ssh_key("alice", &fp2).await.unwrap();

    let host_key = gen_host_key(&kd);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let app = common::app(s.clone()).await;
    tokio::spawn(async move { kloudlite_vcs::ssh::serve(app, l, host_key).await.unwrap() });

    let ssh_cmd = format!(
        "ssh -i {} -p {port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        key.display()
    );
    let w = tempfile::tempdir().unwrap();
    let out = std::process::Command::new("git")
        .args([
            "clone",
            "-q",
            &format!("ssh://git@127.0.0.1:{port}/bob/proj.git"),
            "c",
        ])
        .current_dir(w.path())
        .env("GIT_SSH_COMMAND", &ssh_cmd)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("access denied"), "stderr: {err}");
}

/// The SSH twin of `a_node_fenced_by_a_stray_process_reopens_when_it_is_still_the_owner` in
/// routing.rs: a stray opener fenced this node's handle, routing still says the repo is ours, and
/// the next SSH session must reopen and serve rather than fail until some HTTP request evicts.
#[tokio::test(flavor = "multi_thread")]
async fn ssh_serves_after_a_stray_fence_when_still_the_owner() {
    if !common::have_git() || std::process::Command::new("ssh").arg("-V").output().is_err() {
        eprintln!("skip: git/ssh missing");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();

    let kd = tempfile::tempdir().unwrap();
    let key = kd.path().join("id_ed25519");
    assert!(std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let pubkey = std::fs::read_to_string(kd.path().join("id_ed25519.pub")).unwrap();
    let fp = common::ssh_fingerprint(&pubkey).unwrap();
    s.add_ssh_key("alice", &fp).await.unwrap();

    let host_key = gen_host_key(&kd);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let app = common::app(s.clone()).await;
    tokio::spawn(async move { kloudlite_vcs::ssh::serve(app, l, host_key).await.unwrap() });

    // This node holds the repo; a stray opener takes the writer epoch out from under it.
    let held = s.pool.get("alice", "proj").await.unwrap();
    let stray = slatedb::Db::builder(kloudlite_storage::pool::path("alice", "proj"), s.os.clone())
        .build()
        .await
        .unwrap();
    stray.put(b"k", b"v").await.unwrap();
    {
        let mut st = held.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while st.borrow().close_reason.is_none() {
                st.changed().await.unwrap();
            }
        })
        .await
        .expect("the held handle must observe the fence");
    }
    drop(held);
    stray.close().await.unwrap();

    let ssh_cmd = format!(
        "ssh -i {} -p {port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        key.display()
    );
    let url = format!("ssh://git@127.0.0.1:{port}/alice/proj.git");
    let out = std::process::Command::new("git")
        .args(["ls-remote", &url])
        .env("GIT_SSH_COMMAND", &ssh_cmd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ssh after a stray fence: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(s.pool.warm_count(), 1, "reopened, not left fenced");
}

/// Host key via ssh-keygen: ssh-key's `PrivateKey::random` needs a rand_core
/// version this crate doesn't depend on, and the CLI is already required here.
fn gen_host_key(dir: &tempfile::TempDir) -> russh::keys::PrivateKey {
    let p = dir.path().join("host_ed25519");
    assert!(std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", p.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    russh::keys::PrivateKey::from_openssh(std::fs::read_to_string(&p).unwrap()).unwrap()
}

//! Every tool and every `/fs/*` route serves a TREE, and paths are the tree's, never the pod's.
//!
//! Two rules ride together here and the tests keep them apart. Confinement (§4.4): `main`'s root
//! is the workspace and it may not look under `.agents/`; a tree's root is its own subvolume and
//! it may not look outside it. Tree-relative paths (§3.5): what the model gives and what it gets
//! back are both relative to its own working directory, and an absolute path is a 400 — a shape
//! to correct, not a permission to deny.

use kloudlite_ide::paths::{confine, relative};
use kloudlite_ide::sandbox::{bwrap_argv, missing_bind};
use kloudlite_ide::server::App;
use kloudlite_ide::tools::ToolError;
use kloudlite_ide::{Config, TreeCtx};
use std::sync::Arc;

/// A workspace with one tree, `x`, laid out the way the node agent leaves it.
fn workspace() -> (tempfile::TempDir, Arc<App>) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let root = home.join("workspaces/ws-1");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join(".agents/x/src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join(".agents/x/src/main.rs"), "fn main() {}\n").unwrap();
    let cfg = Config { bind: "127.0.0.1:0".parse().unwrap(), root, home, graft_dir: None };
    (tmp, Arc::new(App::new(cfg)))
}

fn main_tree(app: &Arc<App>) -> Arc<TreeCtx> {
    app.tree(None).expect("the main tree always resolves")
}

#[test]
fn an_absolute_path_is_a_400_naming_the_shape_to_correct() {
    let (_t, app) = workspace();
    let t = main_tree(&app);
    let e = confine(&t, "/home/kl/x").unwrap_err();
    assert!(
        matches!(&e, ToolError::Invalid(m) if m.contains("paths are relative to your working directory")),
        "a 400, not a 403: {e:?}"
    );
    // The tree's OWN absolute path is refused the same way. There is one shape, and it is relative.
    let own = t.root.join("src/main.rs").to_string_lossy().into_owned();
    assert!(matches!(confine(&t, &own), Err(ToolError::Invalid(_))), "even its own root spelled absolutely");
}

/// The one place under its own root the main tree may not look. Named in the denial, because the
/// caller can act on a path — and because a bare "denied" reads as a bug in the tool.
#[test]
fn the_main_tree_is_refused_its_own_agents_directory() {
    let (_t, app) = workspace();
    let t = main_tree(&app);
    let e = confine(&t, ".agents/x/src/main.rs").unwrap_err();
    assert!(matches!(&e, ToolError::Denied(m) if m.contains(".agents/x/src/main.rs")), "{e:?}");
    // And by climbing, not only by spelling it out.
    assert!(matches!(confine(&t, "src/../.agents/x"), Err(ToolError::Denied(_))));
}

#[test]
fn a_tree_resolves_its_own_paths_and_cannot_climb_out_of_itself() {
    let (_t, app) = workspace();
    let x = app.tree(Some("x")).expect("tree x exists on disk");
    assert_eq!(confine(&x, "src/a.rs").unwrap(), x.root.join("src/a.rs"));
    // `..` climbing to the workspace — the main tree's files — is refused: a tree's root is the
    // fence, and the main session's work is not the subagent's to read.
    assert!(matches!(confine(&x, "../../src/main.rs"), Err(ToolError::Denied(_))), "a tree cannot reach the workspace");
}

/// The home is no longer an allowed prefix for a tool. It was, so a model could edit dotfiles;
/// that is the person's shell's job now, and the shell is a separate container.
#[test]
fn the_home_is_no_longer_reachable_from_a_tool() {
    let (_t, app) = workspace();
    let t = main_tree(&app);
    assert!(confine(&t, "../../.bashrc").is_err());
}

#[test]
fn an_unknown_tree_is_refused_and_main_is_the_default() {
    let (_t, app) = workspace();
    assert_eq!(app.tree(None).unwrap().name, "main");
    assert_eq!(app.tree(Some("main")).unwrap().name, "main");
    let e = app.tree(Some("nope")).unwrap_err();
    assert!(matches!(&e, ToolError::Invalid(m) if m.contains("nope")), "{e:?}");
}

/// Every path OUT is stripped of the root, so a result, a listing, a diff header and an error all
/// read as the model's own working directory. The root itself is `.`, never the empty string.
#[test]
fn every_path_out_is_relative_to_its_own_tree() {
    let (_t, app) = workspace();
    let t = main_tree(&app);
    assert_eq!(relative(&t, &t.root.join("src/main.rs")), "src/main.rs");
    assert_eq!(relative(&t, &t.root), ".");
    let x = app.tree(Some("x")).unwrap();
    assert_eq!(relative(&x, &x.root.join("src/main.rs")), "src/main.rs", "the tree's root, not the workspace's");
}

/// The argv of spec §4.7, as BUILT. A wrapper is only worth having if what it binds is checkable,
/// so this reads as the list it is rather than as a shape.
///
/// One bind, `/nix`, carries both the store and the profile: the profile is `/nix/profile/current`
/// and the pod mounts exactly one `nix` volume over both. The spec's draft argv also named
/// `{profile}` separately as `/home/kl/.nix-profile`, a path no workspace pod has — every exec on
/// the fleet exited 1 on `bwrap: Can't find source path` until that was removed (2026-09-18).
#[test]
fn the_sandbox_binds_the_tree_the_store_and_nothing_else() {
    let (_t, app) = workspace();
    let x = app.tree(Some("x")).unwrap();
    let root = x.root.to_string_lossy().into_owned();
    let home = format!("{root}/.home");
    let argv = bwrap_argv(&x, &["sh".into(), "-c".into(), "true".into()]);
    assert_eq!(
        argv,
        vec![
            "--unshare-all".to_string(),
            "--share-net".into(),
            "--die-with-parent".into(),
            "--new-session".into(),
            // The same path in and out, so the one string §3.5 concedes to `pwd` stays one string.
            "--bind".into(), root.clone(), root.clone(),
            // The store AND the profile under it: one mount, the one the pod actually has.
            "--ro-bind".into(), "/nix".into(), "/nix".into(),
            "--ro-bind".into(), "/etc/passwd".into(), "/etc/passwd".into(),
            "--ro-bind".into(), "/etc/resolv.conf".into(), "/etc/resolv.conf".into(),
            "--tmpfs".into(), "/tmp".into(),
            "--proc".into(), "/proc".into(),
            "--dev".into(), "/dev".into(),
            // Per tree, so git/npm/cargo config lands in the tree rather than in the person's home.
            "--setenv".into(), "HOME".into(), home,
            "--chdir".into(), root,
            "--".into(), "sh".into(), "-c".into(), "true".into(),
        ]
    );
    // Nothing of the workspace root, the other trees, the token or `kl` is named anywhere.
    assert!(!argv.iter().any(|a| a.ends_with("/workspaces/ws-1")), "{argv:?}");
    // And nothing under the HOME: the home is not bound, so naming a path inside it could only be
    // a source bwrap would refuse to start on — which is the outage this test now stands for.
    assert!(!argv.iter().any(|a| a.starts_with("/home/kl/.")), "{argv:?}");
}

/// Every bind source the wrapper names must be one that exists, and the pair that decides this is
/// what keeps a layout this code does not expect from costing a person their command.
///
/// This is the regression test for the fleet-wide outage: `bwrap` refuses to start when a
/// `--ro-bind` source is missing, and to the caller that refusal is indistinguishable from the
/// command itself failing. Now it costs the sandbox and nothing else.
#[test]
fn a_bind_source_that_is_not_there_costs_the_sandbox_and_not_the_command() {
    // On any machine that runs this, `/nix` may or may not exist — so the assertion is the
    // AGREEMENT between the two functions, not a fixed answer: whatever `missing_bind` names must
    // be a path `bwrap_argv` actually binds, and when it names nothing every source must be real.
    let (_t, app) = workspace();
    let x = app.tree(Some("x")).unwrap();
    let argv = bwrap_argv(&x, &["true".into()]);
    let bound: Vec<&String> = argv
        .windows(3)
        .filter(|w| w[0] == "--ro-bind")
        .map(|w| &w[1])
        .collect();
    assert!(!bound.is_empty(), "the wrapper binds something");
    match missing_bind() {
        Some(p) => assert!(bound.iter().any(|b| *b == p), "{p} is reported missing but never bound: {bound:?}"),
        None => {
            for b in bound {
                assert!(std::path::Path::new(b).exists(), "{b} is bound but missing, and nothing said so");
            }
        }
    }
}

/// What `env` shows a model: the profile's own variables plus the three that are ours. Not `HOME`
/// (bwrap sets it to the tree's own), and never `KL_WORKSPACE` — it names the layout §3.5 says a
/// model is not taught.
#[tokio::test]
async fn an_exec_in_a_tree_carries_its_port_block_and_no_pod_layout() {
    let (_t, app) = workspace();
    let v = app
        .registry
        .call("exec", serde_json::json!({ "cmd": "echo $KL_TREE $PORT $KL_PORT_RANGE; echo [${KL_WORKSPACE}]", "tree": "x" }))
        .await
        .unwrap();
    let out = v["stdout"].as_str().unwrap();
    assert!(out.starts_with("x 20100 20100-20199"), "{out}");
    assert!(out.contains("[]"), "KL_WORKSPACE is not in the environment: {out}");
}

/// The main tree owns the ordinary range and gets no block at all — it is the workspace, and the
/// ports a person already uses are not to be moved out from under them. It still NAMES itself:
/// a session is told which working directory it is in, which is not the same as being told the
/// layout around it.
#[tokio::test]
async fn an_exec_in_main_has_no_port_block() {
    let (_t, app) = workspace();
    let v = app.registry.call("exec", serde_json::json!({ "cmd": "echo [$KL_PORT_RANGE][$KL_TREE]" })).await.unwrap();
    assert_eq!(v["stdout"].as_str().unwrap().trim(), "[][main]");
}

/// A process id names the tree it was started in. Listing from another tree never shows it —
/// a subagent must not be able to kill the main session's dev server, or even see it.
#[tokio::test]
async fn processes_are_scoped_to_the_tree_that_started_them() {
    let (_t, app) = workspace();
    let v = app.registry.call("exec", serde_json::json!({ "cmd": "sleep 30", "detach": true, "tree": "x" })).await.unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let mine = app.registry.call("process_list", serde_json::json!({ "tree": "x" })).await.unwrap();
    assert_eq!(mine["processes"].as_array().unwrap().len(), 1);
    let theirs = app.registry.call("process_list", serde_json::json!({})).await.unwrap();
    assert!(theirs["processes"].as_array().unwrap().is_empty(), "main sees none of the tree's: {theirs}");
    // And cannot reach one by id either.
    assert!(app.registry.call("process_output", serde_json::json!({ "id": id })).await.is_err());
    let _ = app.registry.call("process_kill", serde_json::json!({ "id": id, "tree": "x" })).await;
}

/// The refusal that is the net under the port convention: a model gets a sentence naming the
/// holder rather than a stack trace it has to read a bind error out of.
#[test]
fn a_bind_failure_is_reported_as_a_sentence_naming_the_holder() {
    use kloudlite_ide::procs::port_conflict;
    let ring = b"Error: listen EADDRINUSE: address already in use :::3000\n";
    assert_eq!(port_conflict(ring, "npm run dev -- --port 3000"), Some(3000));
    assert_eq!(port_conflict(b"all good\n", "npm run dev -- --port 3000"), None, "no bind failure, no claim");
    // The hint is read off the command line, in each of the three spellings.
    assert_eq!(port_conflict(ring, "vite -p 3000"), Some(3000));
    assert_eq!(port_conflict(ring, "PORT=3000 node server.js"), Some(3000));
    // A bind failure with no readable port still reports the failure, without inventing a number.
    assert_eq!(port_conflict(ring, "node server.js"), None);
}

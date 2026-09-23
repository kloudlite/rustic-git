//! Every tool and every `/fs/*` route serves a TREE, and paths are the tree's, never the pod's.
//!
//! Two rules ride together here and the tests keep them apart. Confinement (§4.4): `main`'s root
//! is the workspace and it may not look under `.agents/`; a tree's root is its own subvolume and
//! it may not look outside it. Tree-relative paths (§3.5): what the model gives and what it gets
//! back are both relative to its own working directory, and an absolute path is a 400 — a shape
//! to correct, not a permission to deny.

use kloudlite_ide::paths::{confine, relative};
use kloudlite_ide::sandbox::{bwrap_argv, bwrap_argv_with, bwrap_argv_with_cwd, missing_bind};
use kloudlite_ide::server::App;
use kloudlite_ide::tools::ToolError;
use kloudlite_ide::{Config, TreeCtx};
use std::sync::Arc;

/// A workspace with one tree, `x`, laid out the way the node agent leaves it.
fn workspace() -> (tempfile::TempDir, Arc<App>) {
    let tmp = match std::env::var_os("KL_IDE_BWRAP_TEST_TMPDIR") {
        Some(dir) => tempfile::Builder::new().prefix("ide-").tempdir_in(dir).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let home = tmp.path().canonicalize().unwrap();
    let root = home.join("workspaces/ws-1");
    std::fs::create_dir_all(root.join("src")).unwrap();
    // A tree snapshots the whole home, so its copy of the workspace sits at the same path below it.
    let tree = home.join(".agents/x/workspaces/ws-1");
    std::fs::create_dir_all(tree.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(tree.join("src/main.rs"), "fn main() {}\n").unwrap();
    let cfg = Config { bind: "127.0.0.1:0".parse().unwrap(), root, home, graft_dir: None, token_path: None };
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
    let argv = bwrap_argv(&x, &["sh".into(), "-c".into(), "true".into()]);

    // The flags that make it a sandbox, in order, before any bind.
    assert_eq!(
        &argv[..4],
        &["--unshare-all".to_string(), "--share-net".into(), "--die-with-parent".into(), "--new-session".into()]
    );
    // The tree, bound at the same path in and out — the one string §3.5 concedes stays one string.
    let bind = argv.windows(3).find(|w| w[0] == "--bind").expect("the tree is bound");
    assert_eq!((bind[1].as_str(), bind[2].as_str()), (root.as_str(), root.as_str()));
    // The store, and read-only everywhere.
    let sources: Vec<&String> = argv.windows(3).filter(|w| w[0] == "--ro-bind").map(|w| &w[1]).collect();
    assert_eq!(sources[0], "/nix", "{argv:?}");
    // HOME is the tree's own, never the person's.
    let home = argv.windows(3).find(|w| w[0] == "--setenv" && w[1] == "HOME").expect("HOME is set");
    assert_eq!(home[2], format!("{root}/.home"));
    // The command is last, whole, after the `--`.
    let dashdash = argv.iter().position(|s| s == "--").unwrap();
    assert_eq!(&argv[dashdash + 1..], &["sh".to_string(), "-c".into(), "true".into()]);

    // Nothing of the workspace root, the other trees or `kl` is named anywhere.
    // (The tree's own copy ends the same way, one `.agents/x` deeper.)
    assert!(!argv.iter().any(|a| a.ends_with("/workspaces/ws-1") && !a.contains("/.agents/")), "{argv:?}");
    // And nothing under a HOME is a bind SOURCE: the pod has no `~/.nix-profile`, and naming one
    // made bwrap refuse to start on every exec in the fleet (2026-09-18).
    assert!(!sources.iter().any(|s| s.starts_with("/home/kl/.")), "{argv:?}");
    // The token's directory is never inside, whatever else is: reading it is what the wrapper
    // exists to prevent.
    assert!(!sources.iter().any(|s| s.starts_with("/etc/kloudlite")), "{argv:?}");
}

#[test]
fn the_main_sandbox_masks_agent_trees() {
    let (_t, app) = workspace();
    let main = main_tree(&app);
    let agent = app.tree(Some("x")).unwrap();
    let main_argv = bwrap_argv(&main, &["true".into()]);
    let agent_argv = bwrap_argv(&agent, &["true".into()]);
    let main_agents = format!("{}/.agents", main.root.to_string_lossy());
    assert!(main_argv.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == main_agents), "{main_argv:?}");
    assert!(!agent_argv.iter().any(|arg| arg == &format!("{}/.agents", agent.root.to_string_lossy())), "{agent_argv:?}");
}

#[test]
fn the_wrapper_uses_a_nested_working_directory() {
    let (_t, app) = workspace();
    let main = main_tree(&app);
    let cwd = main.root.join("src");
    let argv = bwrap_argv_with_cwd(&main, &["pwd".into()], &[], &cwd);
    let chdir = argv.windows(2).find(|w| w[0] == "--chdir").expect("--chdir");
    assert_eq!(chdir[1], cwd.to_string_lossy());
}

#[test]
#[ignore = "requires a functional bubblewrap runtime"]
fn a_running_main_sandbox_cannot_read_or_write_an_agent_tree() {
    let (_t, app) = workspace();
    let main = main_tree(&app);
    let secret = main.root.join(".agents/x/secret");
    let created = main.root.join(".agents/x/created");
    std::fs::create_dir_all(main.root.join(".agents/x")).unwrap();
    std::fs::write(&secret, "agent secret").unwrap();
    let bwrap = kloudlite_ide::sandbox::usable(&main).expect("functional bwrap required for this integration test");
    let script = format!("mkdir -p '{}/x' && test ! -r '{}' && printf hidden > '{}'", main.root.join(".agents").display(), secret.display(), created.display());
    let argv = bwrap_argv_with_cwd(&main, &["/nix/profile/current/bin/sh".into(), "-c".into(), script], &[], &main.root);
    let output = std::process::Command::new(bwrap).args(&argv).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!created.exists());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "agent secret");

    let nested = main.root.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let argv = bwrap_argv_with_cwd(
        &main,
        &["/nix/profile/current/bin/sh".into(), "-c".into(), "pwd; printf wrapped > sentinel".into()],
        &[],
        &nested,
    );
    let output = std::process::Command::new(bwrap).args(&argv).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), nested.to_string_lossy());
    assert_eq!(std::fs::read_to_string(nested.join("sentinel")).unwrap(), "wrapped");
    assert!(!main.root.join("sentinel").exists());
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
    let bound: Vec<&String> = argv.windows(3).filter(|w| w[0] == "--ro-bind").map(|w| &w[1]).collect();
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

/// A wrapped exec is promised `KL_TREE`, `PORT` and `KL_PORT_RANGE`, so they have to be IN THE
/// ARGV: `Command::env` sets a variable on bwrap, and what bwrap hands the child is its own
/// business — `PORT` came out empty inside a tree's exec for exactly that reason (R-D22).
///
/// Checked on the argv rather than by running one, because CI cannot run a wrapped exec at all —
/// which is how every one of these got out.
#[test]
fn the_wrapper_passes_the_tree_env_to_the_child() {
    let (_t, app) = workspace();
    let x = app.tree(Some("x")).unwrap();
    let env = vec![
        ("KL_TREE".to_string(), "x".to_string()),
        ("PORT".to_string(), "20100".to_string()),
        ("KL_PORT_RANGE".to_string(), "20100-20199".to_string()),
    ];
    let argv = bwrap_argv_with(&x, &["true".into()], &env);
    let set: Vec<(&str, &str)> = argv
        .windows(3)
        .filter(|w| w[0] == "--setenv")
        .map(|w| (w[1].as_str(), w[2].as_str()))
        .collect();
    for (k, v) in [("KL_TREE", "x"), ("PORT", "20100"), ("KL_PORT_RANGE", "20100-20199")] {
        assert!(set.contains(&(k, v)), "{k}={v} is not passed to the child: {set:?}");
    }
    // And never the pod's layout, which §3.5 keeps from a model.
    assert!(!set.iter().any(|(k, _)| *k == "KL_WORKSPACE"), "{set:?}");
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

    // Every runtime spells it differently, and a matcher that knew only Node's wording answered
    // "no conflict" for the rest — which a model reads as a server that died for no reason. These
    // are the real lines, `ttyd`'s copied from a pod (2026-09-18).
    let ttyd = b"[2026/09/18 07:15:28] E: [null wsi]: lws_socket_bind: ERROR on binding fd 10 to port 20001 (-1 48)\n";
    assert_eq!(port_conflict(ttyd, "ttyd -p 20001 -i 127.0.0.1 sleep 600"), Some(20001));
    assert_eq!(port_conflict(b"listen tcp :8080: bind: address already in use\n", "-p 8080"), Some(8080));
    assert_eq!(port_conflict(b"nginx: [emerg] bind() to 0.0.0.0:80 failed (98: Address in use)\n", "--port=80"), Some(80));
    assert_eq!(port_conflict(b"Failed to bind to /0.0.0.0:9090\n", "PORT=9090 java -jar app.jar"), Some(9090));
    // And nothing that is not a bind failure is read as one, whatever the command line says.
    assert_eq!(port_conflict(b"compiling, binding the template to the model\n", "-p 3000"), None);
}

/// A walk DISCOVERS paths rather than being handed them, so it prunes where `confine` refuses.
/// `read` of `.agents/probe/x` from main is a 403 — and `glob`, `grep` and `/fs/tree` must not
/// list the same file, or the fence holds only for whoever already knows the path.
///
/// The fleet found this: `ws.tree.cut` compared main's listing to the tree's and main's carried
/// `.agents/probe/…` (2026-09-18). One predicate now, so a fourth walk cannot forget.
#[tokio::test]
async fn no_walk_on_the_main_tree_shows_another_sessions_files() {
    let (_t, app) = workspace();
    // Something to find under both roots, with the same name, so a hit is unambiguous.
    std::fs::write(app.tree(None).unwrap().root.join("marker.rs"), "// main\n").unwrap();
    std::fs::write(app.tree(Some("x")).unwrap().root.join("marker.rs"), "// tree\n").unwrap();

    let hits = |v: &serde_json::Value| -> Vec<String> {
        v["paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect()
    };
    let main = app.registry.call("glob", serde_json::json!({ "pattern": "**/*.rs" })).await.unwrap();
    let found = hits(&main);
    assert!(found.iter().any(|p| p == "marker.rs"), "main finds its own: {found:?}");
    assert!(!found.iter().any(|p| p.contains(".agents")), "main's glob shows a tree's files: {found:?}");

    // The tree finds its own — the fixture gives it a `src/main.rs` of its own too — and nothing
    // of main's, because its root is the fence and main's files are simply not under it.
    let inside = app.registry.call("glob", serde_json::json!({ "pattern": "**/*.rs", "tree": "x" })).await.unwrap();
    let mut inside_hits = hits(&inside);
    inside_hits.sort();
    assert_eq!(inside_hits, vec!["marker.rs".to_string(), "src/main.rs".to_string()], "{inside}");
    // Same NAMES, different files: the listings are equal, which is exactly what `ws.tree.cut`
    // compares, and it must hold without either side reaching into the other.
    let mut found_sorted = found.clone();
    found_sorted.sort();
    assert_eq!(inside_hits, found_sorted, "a fresh tree lists what its source does");

    // grep walks the same way.
    let g = app.registry.call("grep", serde_json::json!({ "pattern": "tree", "mode": "files" })).await.unwrap();
    let files: Vec<String> = g["files"].as_array().unwrap().iter().map(|f| f.to_string()).collect();
    assert!(!files.iter().any(|f| f.contains(".agents")), "main's grep reaches into a tree: {files:?}");

    // And so does the listing a console renders from.
    let (dir, rows, _) = kloudlite_ide::fs::tree::tree(&app.tree(None).unwrap(), ".", 2).await.unwrap();
    assert_eq!(dir, ".");
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(!names.contains(&".agents"), "main's tree listing shows .agents: {names:?}");
    // The tree's own listing is unaffected: there are no trees inside a tree.
    let (_, rows, _) = kloudlite_ide::fs::tree::tree(&app.tree(Some("x")).unwrap(), ".", 1).await.unwrap();
    assert!(rows.iter().any(|r| r.name == "marker.rs"), "a tree lists its own files");
}

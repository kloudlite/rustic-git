//! Stage 14's subagent trees (spec §4.8): the five `ws.tree.*` ids.
//!
//! Every one reads through the workspace's OWN tool server over loopback, the way an agent
//! session would, rather than through `kubectl exec` into the subvolume directly. That is
//! deliberate: a tree only matters as something a session can act on, and a nested subvolume
//! nobody can address through `tree=` is not the thing being probed. The one exception is
//! `ws.tree.no_travel`, which has to look at a RESTORED copy where no tool server has ever been
//! told about the tree.
//!
//! A sibling file rather than lines inside `experience_ws.rs`, which is already past the size
//! rule, and because these five share one workspace and one helper set of their own.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::{json, Value};

use super::experience_ws::{create, push_then_restore, ws_tool};
use super::workspace::ws_exec;
use super::{api, call, poll_json, post};
use crate::ctx::Ctx;

/// A create plus the server's start, plus the cut. The cut itself is one `btrfs subvolume
/// snapshot` — milliseconds — so almost all of this is the workspace coming up.
const CUT_CEILING: Duration = Duration::from_secs(60);
/// Two writes and two reads through a server that is already up.
const QUICK: Duration = Duration::from_secs(30);
/// A push, a snapshot, a restore and a `ready` wait — the same shape `ws.cache.travels` is given.
const TRAVEL_CEILING: Duration = Duration::from_secs(290);
/// A delete plus one agent pass, and then the workspace's own delete with a live tree on it.
const CLOSED_CEILING: Duration = Duration::from_secs(120);
/// One exec through the tool server.
const EXEC: Duration = Duration::from_secs(20);


/// How long the agent is given to cut or collect a tree. The spec says ready within 10 s; this is
/// the poll bound, not the target — the target is the catalogue's.
const PASS: Duration = Duration::from_secs(30);

/// The tree every step here works in. One name, so a failure names the same tree in the log as on
/// disk.
const TREE: &str = "probe";

/// What the holding listener runs as its "shell": a command that simply waits, so `ttyd` stays up
/// holding the port instead of exiting the moment a client would have disconnected.
const SLEEP: &str = "sleep 600";

/// The five `ws.tree.*` ids, in one workspace. Written as one walk rather than five because a
/// tree cannot be probed without a workspace that has one, and creating five would spend four
/// creates on setup.
///
/// Every id is reported even when an earlier one failed: a skip reads as a pass on this fleet
/// (2026-09-09), so a step that cannot run says so by name.
pub async fn trees(c: &mut Ctx) {
    const IDS: [&str; 5] =
        ["ws.tree.cut", "ws.tree.isolated", "ws.tree.no_travel", "ws.tree.ports", "ws.tree.closed"];
    if c.kube.is_none() {
        for id in IDS {
            c.skip(id, "no kubeconfig");
        }
        return;
    }
    let name = format!("{}-tree", c.prefix());
    let cut = c
        .step("ws.tree.cut", CUT_CEILING, |c| {
            let name = name.clone();
            async move {
                let id = create(c, &name, json!({ "packages": [] })).await?;
                c.state.extra_workspaces.push(id.clone());
                wait_for_server(c, &id).await?;
                ask_for_tree(c, &id, TREE).await?;
                wait_ready(c, &id, TREE).await?;
                // The tree carries the source's files: a snapshot, not an empty subvolume. The
                // workspace image ships a home with dotfiles, so `.` is never empty in `main`
                // either — compare the two listings rather than asserting non-emptiness.
                let in_main = list(c, &id, None).await?;
                let in_tree = list(c, &id, Some(TREE)).await?;
                if in_tree != in_main {
                    return Err(anyhow!("the tree listed {in_tree:?}, the workspace {in_main:?}"));
                }
                // And the fence in the other direction: `main` may not read a subagent's files.
                // A 403 NAMING the path, never a bare denial and never a 404 that reads as "no
                // such file" to a model that would then create one. Trees sit in `~/.agents`, one
                // level above main's root (`~/workspace`), so `..` is the way a model would reach.
                let (code, body) = ws_tool(c, &id, "read", &json!({ "path": "../.agents/probe/x" })).await?;
                if code != 403 || !body.contains("../.agents/probe/x") {
                    return Err(anyhow!("main read under .agents answered {code}: {}", body.trim()));
                }
                Ok(())
            }
            .boxed()
        })
        .await;
    let Some(id) = c.state.extra_workspaces.last().cloned().filter(|_| cut) else {
        for id in &IDS[1..] {
            c.skip(id, "no workspace with a tree to probe");
        }
        return;
    };

    // Both directions, in one step: a tree that could read `main`'s new files would be a fence
    // that only holds while nobody writes.
    let ws = id.clone();
    c.step("ws.tree.isolated", QUICK, move |c| {
        let id = ws.clone();
        async move {
            write(c, &id, Some(TREE), "in-tree.txt", "from the tree").await?;
            write(c, &id, None, "in-main.txt", "from main").await?;
            let (code, body) = ws_tool(c, &id, "read", &json!({ "path": "in-tree.txt" })).await?;
            if code == 200 {
                return Err(anyhow!("main can read the tree's file: {}", body.trim()));
            }
            let (code, body) =
                ws_tool(c, &id, "read", &json!({ "path": "in-main.txt", "tree": TREE })).await?;
            if code == 200 {
                return Err(anyhow!("the tree can read main's file: {}", body.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;

    let ws = id.clone();
    c.step("ws.tree.ports", QUICK, move |c| {
        let id = ws.clone();
        async move {
            // The convention: `PORT` and `KL_PORT_RANGE` are the tree's own block, and the number
            // is inside the range rather than merely set.
            let (code, body) = ws_tool(
                c,
                &id,
                "exec",
                &json!({ "cmd": "echo $PORT $KL_PORT_RANGE", "tree": TREE }),
            )
            .await?;
            if code != 200 {
                return Err(anyhow!("the exec answered {code}: {}", body.trim()));
            }
            let out = field(&body, "stdout")?;
            let mut words = out.split_whitespace();
            let (Some(port), Some(range)) = (words.next(), words.next()) else {
                return Err(anyhow!("the tree's exec printed no port block: {out:?}"));
            };
            let port: u16 = port.parse().with_context(|| format!("PORT was {port:?}"))?;
            let (lo, hi) = range
                .split_once('-')
                .ok_or_else(|| anyhow!("KL_PORT_RANGE was {range:?}"))?;
            let (lo, hi): (u16, u16) = (lo.parse()?, hi.parse()?);
            if !(lo..=hi).contains(&port) || lo < 20_000 {
                return Err(anyhow!("PORT {port} is not inside the tree's block {lo}-{hi}"));
            }
            // And the net under it: a listener on a port `main` already holds is reported as a
            // sentence naming the port, never left for the model to read out of a stack trace.
            //
            // `ttyd`, not `nc`: the base profile carries no netcat and no python (`WS_BASE_PACKAGES`
            // — `sh: nc: command not found` on the fleet, 2026-09-18), and this probe must not be
            // the reason a package is added. `ttyd` is in the profile because the shell sidecar
            // runs it, it binds a TCP port, and it prints a bind failure and exits — which is all a
            // holder has to do.
            let held = 20_001;
            let listener = |port: u16| format!("ttyd -p {port} -i 127.0.0.1 {SLEEP}");
            let (code, _) =
                ws_tool(c, &id, "exec", &json!({ "cmd": listener(held), "detach": true })).await?;
            if code != 200 {
                // A holder that cannot start is a gap in the PROBE, not a breach of the SLI — and
                // the two must never be reported as the same thing.
                return Err(anyhow!("could not start the holding listener (is ttyd in the profile?)"));
            }
            // Long enough for ttyd to have bound before the second one tries: a race here would
            // report "no conflict" for a fence that is working.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let (_, body) =
                ws_tool(c, &id, "exec", &json!({ "cmd": listener(held), "detach": true, "tree": TREE })).await?;
            let pid = field(&body, "id")?;
            tokio::time::sleep(Duration::from_secs(3)).await;
            let (_, body) =
                ws_tool(c, &id, "process_output", &json!({ "id": pid, "tree": TREE })).await?;
            if !body.contains(&format!("port {held} in use")) {
                return Err(anyhow!("a clashing listener was not reported as a port conflict: {}", body.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;

    // Before the close, because it needs the tree standing: push this workspace and restore it,
    // then look at the copy. `btrfs send` does not carry a nested subvolume, so the restored
    // `.agents/probe` is an empty DIRECTORY — present, and holding nothing.
    let ws = id.clone();
    let restored = format!("{}-tree-r", c.prefix());
    c.step("ws.tree.no_travel", TRAVEL_CEILING, move |c| {
        let (id, restored) = (ws.clone(), restored.clone());
        async move {
            write(c, &id, Some(TREE), "only-here.txt", "a subagent's work").await?;
            let copy = push_then_restore(c, &id, &restored, "trees").await?;
            // Read with `kubectl exec`, not through the tool server: no tree was ever asked for
            // on the copy, so `tree=probe` would rightly 400 there. What is being checked is the
            // BYTES, and only a shell in the pod can see them.
            // Three outcomes, told apart: the directory is there and EMPTY (the mount point
            // `btrfs send` left behind — a pass), it is there and holds something (the tree
            // travelled — a fail), or it is not there at all (also a pass: nothing of the tree
            // survived). `ls ... || true` alone conflated the third with the second, because the
            // error text landed in stdout and read as content.
            // Trees are cut at the top of the worktree, the home, not under `$KL_WORKSPACE`.
            let script = format!("d={}/.agents/probe; if [ -d \"$d\" ]; then echo PRESENT; ls -A \"$d\"; else echo ABSENT; fi", kloudlite_workspaces::k8s::HOME_DIR);
            let (code, out, err) = ws_exec(c, &copy, &script, EXEC).await?;
            if code != 0 {
                return Err(anyhow!("could not look at the restored .agents/probe: exit {code}: {}", err.trim()));
            }
            let mut lines = out.lines();
            let present = lines.next().unwrap_or("").trim() == "PRESENT";
            let held: Vec<&str> = lines.map(str::trim).filter(|l| !l.is_empty()).collect();
            if present && !held.is_empty() {
                return Err(anyhow!("a tree travelled with the push; the copy holds {held:?}"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;

    let ws = id.clone();
    c.step("ws.tree.closed", CLOSED_CEILING, move |c| {
        let id = ws.clone();
        async move {
            let url = api(c, &format!("/v1/workspaces/{id}/trees/{TREE}"));
            call(c, reqwest::Method::DELETE, &url, &c.probe_jwt, None).await.context("could not delete the tree")?;
            // Gone from status AND from disk: the row dropping while the subvolume stays would
            // leave bytes nothing ever collects.
            let doc = api(c, &format!("/v1/workspaces/{id}"));
            poll_json(c, &doc, &c.probe_jwt, PASS, |v| !names(v).iter().any(|n| n == TREE))
                .await
                .context("the tree never left status.trees")?;
            let (_, out, _) = ws_exec(c, &id, &format!("ls -A {}/.agents 2>&1 || true", kloudlite_workspaces::k8s::HOME_DIR), EXEC).await?;
            if out.contains(TREE) {
                return Err(anyhow!("the subvolume is still on disk: {:?}", out.trim()));
            }
            // And the other half of §4.8: a workspace with a LIVE tree still deletes. btrfs
            // refuses to delete a subvolume with children, so a finalizer that did not sweep the
            // trees first would wedge here rather than anywhere a person would look.
            ask_for_tree(c, &id, "leftover").await?;
            wait_ready(c, &id, "leftover").await?;
            let url = api(c, &format!("/v1/workspaces/{id}"));
            call(c, reqwest::Method::DELETE, &url, &c.probe_jwt, None).await.context("the workspace refused to delete with a live tree")?;
            poll_json(c, &url, &c.probe_jwt, PASS, |v| v.get("id").is_none()).await.ok();
            Ok(())
        }
        .boxed()
    })
    .await;
}

/// `status.trees`' names, or none — a workspace that has never held one carries no key at all.
fn names(v: &Value) -> Vec<String> {
    v.get("trees")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(|r| r.get("name").and_then(Value::as_str)).map(str::to_string).collect())
        .unwrap_or_default()
}

/// A tool answer's string field, with the tool's own body in the error: a probe that says only
/// "missing field" sends whoever reads it back to the pod to find out what was there.
fn field(body: &str, key: &str) -> Result<String> {
    let v: Value = serde_json::from_str(body).with_context(|| format!("the tool answered {body:?}"))?;
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no `{key}` in {body}"))
}

async fn ask_for_tree(c: &Ctx, ws: &str, name: &str) -> Result<()> {
    let url = api(c, &format!("/v1/workspaces/{ws}/trees"));
    post(c, &url, &c.probe_jwt, json!({ "name": name })).await.with_context(|| format!("could not ask for tree {name}"))?;
    Ok(())
}

/// Wait for the node to report the cut. `ready`, not merely present: the row appears with
/// `ready: false` while a cut is retrying, and acting on that would probe the API, not the agent.
async fn wait_ready(c: &Ctx, ws: &str, name: &str) -> Result<()> {
    let url = api(c, &format!("/v1/workspaces/{ws}"));
    poll_json(c, &url, &c.probe_jwt, PASS, |v| {
        v.get("trees")
            .and_then(Value::as_array)
            .is_some_and(|rows| {
                rows.iter().any(|r| {
                    r.get("name").and_then(Value::as_str) == Some(name)
                        && r.get("ready").and_then(Value::as_bool) == Some(true)
                })
            })
    })
    .await
    .with_context(|| format!("tree {name} never turned ready"))?;
    Ok(())
}

/// The tool server has to be up before any of this means anything; `ide.serve.up` owns that as an
/// SLI, and this is the same wait so a tree step never fails for the tool server's reason.
async fn wait_for_server(c: &Ctx, ws: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        let (code, out, err) = ws_exec(c, ws, "curl -sf http://127.0.0.1:7788/healthz", EXEC).await?;
        if code == 0 && out.contains("\"ok\":true") {
            return Ok(());
        }
        last = format!("exit {code}: {} {}", out.trim(), err.trim());
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(anyhow!("the tool server never answered /healthz; last: {last}"))
}

/// Every entry a tree lists at its root, sorted. Through `glob`, so it is the tool server's own
/// view — the one a session gets — rather than a shell's.
async fn list(c: &Ctx, ws: &str, tree: Option<&str>) -> Result<Vec<String>> {
    let mut args = json!({ "pattern": "*" });
    if let (Some(t), Some(o)) = (tree, args.as_object_mut()) {
        o.insert("tree".into(), json!(t));
    }
    let (code, body) = ws_tool(c, ws, "glob", &args).await?;
    if code != 200 {
        return Err(anyhow!("glob answered {code}: {}", body.trim()));
    }
    let v: Value = serde_json::from_str(&body)?;
    let mut paths: Vec<String> = v
        .get("paths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    paths.sort();
    Ok(paths)
}

async fn write(c: &Ctx, ws: &str, tree: Option<&str>, path: &str, content: &str) -> Result<()> {
    let mut args = json!({ "path": path, "content": content });
    if let (Some(t), Some(o)) = (tree, args.as_object_mut()) {
        o.insert("tree".into(), json!(t));
    }
    let (code, body) = ws_tool(c, ws, "write", &args).await?;
    if code != 200 {
        return Err(anyhow!("write of {path} answered {code}: {}", body.trim()));
    }
    Ok(())
}

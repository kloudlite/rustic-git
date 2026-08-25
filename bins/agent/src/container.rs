//! Per-workspace container lifecycle: every materialized workspace runs a container
//! (`ws-{id}`) with its live subvolume bind-mounted, `docker exec` being the v1 access path
//! (see `crates/workspaces/src/model.rs`'s `default_ws_image` doc). Small enough to be a
//! sibling to `engine::compose` (which drives the same `docker` binary for environments) rather
//! than folding into it — an env is a multi-service compose project, a workspace is one plain
//! container, and the two have no shared rendering step.

use rustic_git_workspaces::engine::EngErr;
use std::path::Path;

pub fn name(ws_id: &str) -> String {
    format!("ws-{ws_id}")
}

fn run(argv: &[&str]) -> Result<std::process::Output, EngErr> {
    std::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| EngErr(format!("spawn {}: {e}", argv[0])))
}

/// True if a container (running or stopped) exists with this name.
fn exists(cname: &str) -> Result<bool, EngErr> {
    let out = run(&["docker", "inspect", cname])?;
    Ok(out.status.success())
}

/// Idempotent: starts the existing `ws-{id}` container if one exists (any state), otherwise
/// creates and runs a fresh one. The double bind mount is deliberate — `/workspace` is the
/// generic contract every image can rely on, while also mounting the SAME subvolume read-only
/// at nginx's web root means the default image (`nginx:alpine`) serves the workspace's own
/// files with zero configuration, instead of an empty landing page.
pub fn start(ws_id: &str, image: &str, live: &Path) -> Result<(), EngErr> {
    let cname = name(ws_id);
    if exists(&cname)? {
        let out = run(&["docker", "start", &cname])?;
        return ok(out, "docker start");
    }
    let live_str = live.to_str().ok_or_else(|| EngErr("live path is not valid UTF-8".into()))?;
    let out = run(&[
        "docker",
        "run",
        "-d",
        "--name",
        &cname,
        "--restart",
        "unless-stopped",
        "-v",
        &format!("{live_str}:/workspace"),
        "-v",
        &format!("{live_str}:/usr/share/nginx/html:ro"),
        image,
    ])?;
    ok(out, "docker run")
}

pub fn stop(ws_id: &str) -> Result<(), EngErr> {
    let out = run(&["docker", "stop", &name(ws_id)])?;
    ok(out, "docker stop")
}

/// True if a container by this exact name is currently running — `WsClone`'s branch point
/// between `Engine::clone_running` (source live, pause-around-copy) and `Engine::clone_local`
/// (source stopped, no downtime to manage). A missing container counts as not running rather
/// than an error: a never-started source clones the same way a stopped one does.
pub fn is_running(cname: &str) -> Result<bool, EngErr> {
    let out = run(&["docker", "inspect", "-f", "{{.State.Running}}", cname])?;
    Ok(String::from_utf8_lossy(&out.stdout).trim() == "true")
}

/// Force-remove, ignoring "no such container" — `WsDelete` calls this before the subvolume
/// itself is removed, and a workspace that was never started (or already reaped) is not an
/// error.
pub fn remove(ws_id: &str) -> Result<(), EngErr> {
    let out = run(&["docker", "rm", "-f", &name(ws_id)])?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("No such container") {
        return Ok(());
    }
    Err(EngErr(format!("docker rm -f {}: {stderr}", name(ws_id))))
}

fn ok(out: std::process::Output, what: &str) -> Result<(), EngErr> {
    if out.status.success() {
        Ok(())
    } else {
        Err(EngErr(format!("{what}: {}", String::from_utf8_lossy(&out.stderr))))
    }
}

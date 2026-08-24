//! Renders an `Environment`'s services into a `docker-compose.yml` and drives `docker compose`
//! for it. One compose project per environment (`env-{id}`), so `up`/`down`/`ps` all scope to
//! it — that project name is also what `WsClone`'s `stop_projects` hook (`bins/agent/src/lib.rs`)
//! already knows how to stop/start.

use crate::engine::EngErr;
use crate::model::Environment;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct ComposeFile {
    services: BTreeMap<String, ComposeService>,
}

#[derive(Serialize)]
struct ComposeService {
    image: String,
    command: Vec<String>,
    environment: BTreeMap<String, String>,
    volumes: Vec<String>,
}

pub fn project(env: &Environment) -> String {
    format!("env-{}", env.id)
}

/// One bind volume per service mount, resolved against `mounts` (workspace id -> live dir).
/// A service mount naming a workspace not present in `mounts` is a caller bug (the agent
/// collects one entry per mount before calling `up`), so it's a hard error, not a skip.
fn render(env: &Environment, mounts: &[(String, PathBuf)]) -> Result<String, EngErr> {
    let mut services = BTreeMap::new();
    for svc in &env.services {
        let mut volumes = Vec::new();
        for m in &svc.mounts {
            let (_, live) = mounts
                .iter()
                .find(|(id, _)| *id == m.workspace)
                .ok_or_else(|| EngErr::other(format!("no local mount for workspace {}", m.workspace)))?;
            volumes.push(format!("{}:{}", live.display(), m.path));
        }
        services.insert(
            svc.name.clone(),
            ComposeService { image: svc.image.clone(), command: svc.command.clone(), environment: svc.env.clone().into_iter().collect(), volumes },
        );
    }
    serde_yaml::to_string(&ComposeFile { services }).map_err(|e| EngErr::other(e.to_string()))
}

fn file_path(dir: &Path) -> PathBuf {
    dir.join("docker-compose.yml")
}

fn run(argv: &[&str]) -> Result<(), EngErr> {
    let out = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| EngErr::other(format!("spawn {}: {e}", argv[0])))?;
    if !out.status.success() {
        return Err(EngErr::other(format!("{argv:?}: {}", String::from_utf8_lossy(&out.stderr))));
    }
    Ok(())
}

/// Renders `docker-compose.yml` into `dir` and brings the project up detached.
pub fn up(env: &Environment, mounts: &[(String, PathBuf)], dir: &Path) -> Result<(), EngErr> {
    std::fs::create_dir_all(dir).map_err(|e| EngErr::other(e.to_string()))?;
    let yaml = render(env, mounts)?;
    let path = file_path(dir);
    std::fs::write(&path, yaml).map_err(|e| EngErr::other(e.to_string()))?;
    run(&["docker", "compose", "-p", &project(env), "-f", path.to_str().unwrap(), "up", "-d"])
}

/// Tears the project down. The compose file must already exist at `dir` (written by `up`).
pub fn down(env: &Environment, dir: &Path) -> Result<(), EngErr> {
    let path = file_path(dir);
    run(&["docker", "compose", "-p", &project(env), "-f", path.to_str().unwrap(), "down"])
}

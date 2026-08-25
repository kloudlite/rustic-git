//! Renders an `Environment`'s services into a `docker-compose.yml` and drives `docker compose`
//! for it. One compose project per environment (`env-{id}`), so `up`/`down`/`ps` all scope to
//! it — that project name is also what `WsClone`'s `stop_projects` hook (`bins/agent/src/lib.rs`)
//! already knows how to stop/start.
//!
//! An environment owns exactly ONE subvolume; every declared volume is a folder inside its
//! `live/volumes/{name}` — so every service mount bind-mounts a path under the SAME `live` dir,
//! not a different workspace's.

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

/// One bind volume per service mount, resolved against `live` (the env's single subvolume) —
/// `live/volumes/{mount.folder}` bind-mounted at `mount.path`. `EnvUp` mkdir -p's every
/// declared folder before calling this, so the bind source always exists.
fn render(env: &Environment, live: &Path) -> Result<String, EngErr> {
    let mut services = BTreeMap::new();
    for svc in &env.services {
        let mut volumes = Vec::new();
        for m in &svc.mounts {
            let vol_dir = live.join("volumes").join(&m.folder);
            volumes.push(format!("{}:{}", vol_dir.display(), m.path));
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

/// Renders `docker-compose.yml` into `dir` and brings the project up detached. `live` is the
/// env's own subvolume — every mount resolves under it (see `render`).
pub fn up(env: &Environment, live: &Path, dir: &Path) -> Result<(), EngErr> {
    std::fs::create_dir_all(dir).map_err(|e| EngErr::other(e.to_string()))?;
    let yaml = render(env, live)?;
    let path = file_path(dir);
    std::fs::write(&path, yaml).map_err(|e| EngErr::other(e.to_string()))?;
    run(&["docker", "compose", "-p", &project(env), "-f", path.to_str().unwrap(), "up", "-d"])
}

/// Tears the project down. The compose file must already exist at `dir` (written by `up`).
pub fn down(env: &Environment, dir: &Path) -> Result<(), EngErr> {
    let path = file_path(dir);
    run(&["docker", "compose", "-p", &project(env), "-f", path.to_str().unwrap(), "down"])
}

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
            // Belt to the API's braces: this is the last place before a string becomes a host
            // bind, and the agent that acts on it runs as root.
            crate::model::validate_mount(m).map_err(EngErr::other)?;
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

#[cfg(test)]
mod tests {
    use super::render;
    use crate::model::{EnvState, Environment, Mount, Service};
    use std::path::Path;

    fn env_with(folder: &str, path: &str) -> Environment {
        Environment {
            id: "env-1".into(),
            owner: "alice".into(),
            name: "dev".into(),
            region: "centralindia".into(),
            state: EnvState::Creating,
            placement: None,
            volume: None,
            services: vec![Service {
                name: "web".into(),
                image: "nginx".into(),
                command: vec![],
                env: Default::default(),
                mounts: vec![Mount { folder: folder.into(), path: path.into() }],
            }],
        }
    }

    #[test]
    fn render_refuses_a_mount_that_escapes_the_subvolume() {
        let live = Path::new("/mnt/wspool/vol/env-1/live");
        let ok = render(&env_with("data", "/data"), live).unwrap();
        assert!(ok.contains("/mnt/wspool/vol/env-1/live/volumes/data:/data"));

        // A doc written before the API validated mounts (or a store edited out of band) must not
        // become a host bind here.
        for bad in ["/", "..", "a/b", ""] {
            assert!(render(&env_with(bad, "/host"), live).is_err(), "folder {bad:?} must be refused");
        }
        assert!(render(&env_with("data", "/data:ro"), live).is_err());
    }
}

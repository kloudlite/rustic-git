//! Everything that becomes a `docker` process. The argv builders are pure so they can be checked
//! without a docker binary; the runners below them are thin.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The buildx builder every workspace uses; `kl-build.sh` creates the same one for a login shell.
pub const BUILDER: &str = "kl";
/// How long a cold builder may take before `kl build` gives up waiting for it — the gate's
/// `builder_start_secs` (120) plus buildx's own dial, under the probe's 180 s ceiling.
pub const WAIT: Duration = Duration::from_secs(150);

pub fn build_argv(
    refs: &[String],
    file: Option<&str>,
    build_args: &[String],
    platform: Option<&str>,
    no_cache: bool,
    context: &str,
    metadata_file: &str,
) -> Vec<String> {
    let mut v: Vec<String> = ["buildx", "build", "--builder", BUILDER, "--push", "--metadata-file", metadata_file]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for r in refs {
        v.push("-t".into());
        v.push(r.clone());
    }
    if let Some(f) = file {
        v.push("-f".into());
        v.push(f.into());
    }
    for a in build_args {
        v.push("--build-arg".into());
        v.push(a.clone());
    }
    if let Some(p) = platform {
        v.push("--platform".into());
        v.push(p.into());
    }
    if no_cache {
        v.push("--no-cache".into());
    }
    v.push(context.into());
    v
}

pub fn promote_argv(src: &str, dst: &str) -> Vec<String> {
    ["buildx", "imagetools", "create", "-t", dst, src].iter().map(|s| s.to_string()).collect()
}

fn quiet(args: &[&str]) -> Result<bool, String> {
    Command::new("docker")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .map_err(|e| format!("docker: {e}"))
}

/// `docker buildx create` unless the builder already exists. Idempotent; a login shell's
/// `kl-build.sh` may have made it first.
pub fn ensure_builder(buildkit_host: &str) -> Result<(), String> {
    if quiet(&["buildx", "inspect", BUILDER])? {
        return Ok(());
    }
    if quiet(&["buildx", "create", "--name", BUILDER, "--driver", "remote", buildkit_host])? {
        Ok(())
    } else {
        Err("could not create the kl buildx builder".into())
    }
}

/// `~/.docker/config.json` naming the credential helper for the registry host — written only
/// when absent, so a person's own docker config is never overwritten.
pub fn ensure_cred_helper(home: &Path, host: &str) -> Result<(), String> {
    let dir = home.join(".docker");
    let p = dir.join("config.json");
    if p.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(&p, format!("{{\"credHelpers\":{{\"{host}\":\"kl\"}}}}\n")).map_err(|e| format!("{}: {e}", p.display()))
}

/// `docker buildx inspect --bootstrap`, retried for `WAIT`: buildx's remote driver dials the
/// gate with its own ~20 s deadline, and a cold builder takes longer than that.
pub fn wait_builder() -> Result<(), String> {
    let start = Instant::now();
    let mut said = false;
    loop {
        if quiet(&["buildx", "inspect", "--bootstrap", BUILDER])? {
            return Ok(());
        }
        if start.elapsed() >= WAIT {
            return Err(format!("the builder did not answer within {} s", WAIT.as_secs()));
        }
        if !said {
            eprintln!("starting your builder…");
            said = true;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

pub fn run(argv: &[String]) -> Result<i32, String> {
    let st = Command::new("docker").args(argv).status().map_err(|e| format!("docker: {e}"))?;
    Ok(st.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn build_argv_pushes_on_the_kl_builder_with_every_flag_passed_through() {
        let argv = build_argv(
            &s(&["cr.khost.dev/alice/hello:1", "cr.khost.dev/alice/hello:latest"]),
            Some("Dockerfile.prod"),
            &s(&["A=1", "B=2"]),
            Some("linux/amd64"),
            true,
            "./app",
            "/tmp/kl-meta.json",
        );
        assert_eq!(
            argv,
            s(&[
                "buildx", "build", "--builder", "kl", "--push",
                "--metadata-file", "/tmp/kl-meta.json",
                "-t", "cr.khost.dev/alice/hello:1", "-t", "cr.khost.dev/alice/hello:latest",
                "-f", "Dockerfile.prod", "--build-arg", "A=1", "--build-arg", "B=2",
                "--platform", "linux/amd64", "--no-cache", "./app",
            ])
        );
    }

    #[test]
    fn build_argv_with_defaults_is_minimal() {
        let argv = build_argv(&s(&["cr.khost.dev/alice/hello:latest"]), None, &[], None, false, ".", "/tmp/m");
        assert_eq!(argv, s(&["buildx", "build", "--builder", "kl", "--push", "--metadata-file", "/tmp/m", "-t", "cr.khost.dev/alice/hello:latest", "."]));
    }

    #[test]
    fn promote_is_a_registry_side_copy() {
        assert_eq!(
            promote_argv("cr.khost.dev/alice/hello:1", "cr.khost.dev/alice/hello:latest"),
            s(&["buildx", "imagetools", "create", "-t", "cr.khost.dev/alice/hello:latest", "cr.khost.dev/alice/hello:1"])
        );
    }

    #[test]
    fn cred_helper_config_is_written_once_and_never_overwritten() {
        let home = tempfile::tempdir().unwrap();
        ensure_cred_helper(home.path(), "cr.khost.dev").unwrap();
        let p = home.path().join(".docker/config.json");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"credHelpers\":{\"cr.khost.dev\":\"kl\"}}\n");
        std::fs::write(&p, "{\"auths\":{}}").unwrap();
        ensure_cred_helper(home.path(), "cr.khost.dev").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"auths\":{}}");
    }
}

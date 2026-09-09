# Workspace `kl` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `kl` binary in every workspace with two verbs — `kl build` (build on the owner's builder, push to the owner's registry) and `kl push` (promote an image already in the registry) — exercised by the hourly probe.

**Architecture:** A new dependency-light crate `bins/kl` (clap only) that execs the docker CLI already in the workspace image, built for `x86_64-unknown-linux-musl` because the workspace image is Alpine. Reference expansion is a pure function; every docker invocation is built as argv by a pure function; both are unit-tested. The probe's `ws.build.p95` switches to `kl build`, and a new `ws.build.promote` covers `kl push`.

**Tech Stack:** Rust 2021, clap 4 (workspace dep), `std::process::Command`; docker CLI + buildx (already in the image); the existing SLO probe (`bins/slo`).

**Spec:** `docs/superpowers/specs/2026-09-09-workspace-kl-design.md`

## Global Constraints

- The laptop CLI is `kl-connect` (commit `f5663b1c`); the workspace binary is `kl`. No shared crate, no shared code.
- `bins/kl` depends on `clap` only. No tokio, reqwest, TLS, serde.
- `kl` holds no token and calls no `/v1` route. Credentials are the docker credential helper's business.
- Reference expansion table (spec §3): `hello` → `{host}/{owner}/hello:latest`; `hello:1` → `{host}/{owner}/hello:1`; `acme/hello:1` → `{host}/acme/hello:1`; a first segment containing `.` or `:` is left unchanged.
- `kl build` requires at least one `-t`. `kl push` with no destination is refused with exactly: `a build pushes as it finishes — \`kl build -t hello:1 .\`; \`kl push\` copies an image the registry already has to another name`.
- Builder bootstrap wait: retry `docker buildx inspect --bootstrap kl` for up to 150 s, 5 s apart.
- SLO ids are read BY RESULT from `default.otel_logs`; a skip is not a pass.
- Commit subjects imperative sentence case, no tool attribution. Never run `cargo fmt -p`; the repo is not rustfmt-clean.
- Every cargo command runs in the dev pod (`deploy/dev/exec.sh`), never on the laptop.

---

### Task 1: The `kl` crate — expansion, argv, and the two verbs

**Files:**
- Create: `bins/kl/Cargo.toml`
- Create: `bins/kl/src/main.rs`
- Create: `bins/kl/src/refs.rs`
- Create: `bins/kl/src/docker.rs`
- Modify: `Cargo.toml` (workspace members, both lists that name `bins/kl-connect`)

**Interfaces:**
- Produces: `refs::expand(given: &str, host: &str, owner: &str) -> String`
- Produces: `docker::build_argv(refs: &[String], file: Option<&str>, build_args: &[String], platform: Option<&str>, no_cache: bool, context: &str, metadata_file: &str) -> Vec<String>`
- Produces: `docker::promote_argv(src: &str, dst: &str) -> Vec<String>`
- Produces: `docker::ensure_builder(buildkit_host: &str) -> Result<(), String>`, `docker::ensure_cred_helper(home: &Path, host: &str) -> Result<(), String>`, `docker::wait_builder() -> Result<(), String>`
- Produces: binary `kl` with subcommands `build` and `push`.

- [ ] **Step 1: Cargo manifest and workspace membership**

`bins/kl/Cargo.toml`:

```toml
[package]
name = "kl"
version = "0.1.0"
edition = "2021"
license = "SSPL-1.0"
description = "kloudlite workspace CLI: build on the owner's builder, push to the owner's registry"
repository = "https://github.com/kloudlite/rustic-git"

[[bin]]
name = "kl"
path = "src/main.rs"

[dependencies]
clap = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

In the root `Cargo.toml`, add `"bins/kl"` after `"bins/kl-connect"` in BOTH member lists (line 5 and line 15 today).

- [ ] **Step 2: Write the failing tests for `expand`**

`bins/kl/src/refs.rs`:

```rust
//! Reference expansion: what a person types → what is pushed. The owner never types the registry
//! host or their own slug; a team member types `team/name`; anything that already names a host
//! (docker's own rule: a first segment with a `.` or a `:`) is left exactly as given.

/// `hello` → `host/owner/hello:latest`, `hello:1` → `host/owner/hello:1`,
/// `acme/hello:1` → `host/acme/hello:1`, `ghcr.io/x/y:1` → unchanged.
pub fn expand(given: &str, host: &str, owner: &str) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::expand;
    const H: &str = "cr.khost.dev";

    #[test]
    fn a_bare_name_gets_host_owner_and_latest() {
        assert_eq!(expand("hello", H, "alice"), "cr.khost.dev/alice/hello:latest");
    }

    #[test]
    fn a_name_with_a_tag_keeps_the_tag() {
        assert_eq!(expand("hello:1", H, "alice"), "cr.khost.dev/alice/hello:1");
    }

    #[test]
    fn an_owner_prefix_replaces_the_callers_own() {
        assert_eq!(expand("acme/hello:1", H, "alice"), "cr.khost.dev/acme/hello:1");
        assert_eq!(expand("acme/hello", H, "alice"), "cr.khost.dev/acme/hello:latest");
    }

    #[test]
    fn a_reference_that_names_a_host_is_unchanged() {
        assert_eq!(expand("ghcr.io/x/y:1", H, "alice"), "ghcr.io/x/y:1");
        assert_eq!(expand("localhost:5000/y", H, "alice"), "localhost:5000/y");
        assert_eq!(expand("cr.khost.dev/alice/hello:2", H, "alice"), "cr.khost.dev/alice/hello:2");
    }

    #[test]
    fn a_digest_reference_is_never_given_a_tag() {
        assert_eq!(expand("hello@sha256:abcd", H, "alice"), "cr.khost.dev/alice/hello@sha256:abcd");
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `deploy/dev/exec.sh cargo test -p kl --lib refs`
Expected: FAIL — `not yet implemented` panics from `todo!()`.

- [ ] **Step 4: Implement `expand`**

Replace the `todo!()`:

```rust
pub fn expand(given: &str, host: &str, owner: &str) -> String {
    let first = given.split('/').next().unwrap_or_default();
    if given.contains('/') && (first.contains('.') || first.contains(':')) {
        return given.to_string();
    }
    let path = if given.contains('/') { given.to_string() } else { format!("{owner}/{given}") };
    // A digest already pins the image; a tag is only defaulted when neither is present. The `:`
    // test looks past the last `/` so a host port never reads as a tag.
    let last = path.rsplit('/').next().unwrap_or_default();
    if last.contains('@') || last.contains(':') {
        format!("{host}/{path}")
    } else {
        format!("{host}/{path}:latest")
    }
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `deploy/dev/exec.sh cargo test -p kl --lib refs`
Expected: 5 passed.

- [ ] **Step 6: Write the failing tests for the docker argv builders**

`bins/kl/src/docker.rs`:

```rust
//! Everything that becomes a `docker` process. The argv builders are pure so they can be checked
//! without a docker binary; the runners below them are thin.

use std::path::Path;
use std::process::Command;
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
    todo!()
}

pub fn promote_argv(src: &str, dst: &str) -> Vec<String> {
    todo!()
}

/// `docker buildx create` unless the builder already exists. Idempotent; a login shell's
/// `kl-build.sh` may have made it first.
pub fn ensure_builder(buildkit_host: &str) -> Result<(), String> {
    todo!()
}

/// `~/.docker/config.json` naming the credential helper for the registry host — written only
/// when absent, so a person's own docker config is never overwritten.
pub fn ensure_cred_helper(home: &Path, host: &str) -> Result<(), String> {
    todo!()
}

/// `docker buildx inspect --bootstrap`, retried for `WAIT`: buildx's remote driver dials the
/// gate with its own ~20 s deadline, and a cold builder takes longer than that.
pub fn wait_builder() -> Result<(), String> {
    todo!()
}

pub fn run(argv: &[String]) -> Result<i32, String> {
    todo!()
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
```

- [ ] **Step 7: Run the tests to see them fail**

Run: `deploy/dev/exec.sh cargo test -p kl --lib docker`
Expected: FAIL with `not yet implemented`.

- [ ] **Step 8: Implement the argv builders and runners**

```rust
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

pub fn ensure_builder(buildkit_host: &str) -> Result<(), String> {
    let exists = Command::new("docker")
        .args(["buildx", "inspect", BUILDER])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("docker: {e}"))?
        .success();
    if exists {
        return Ok(());
    }
    let st = Command::new("docker")
        .args(["buildx", "create", "--name", BUILDER, "--driver", "remote", buildkit_host])
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("docker: {e}"))?;
    if st.success() { Ok(()) } else { Err("could not create the kl buildx builder".into()) }
}

pub fn ensure_cred_helper(home: &Path, host: &str) -> Result<(), String> {
    let dir = home.join(".docker");
    let p = dir.join("config.json");
    if p.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(&p, format!("{{\"credHelpers\":{{\"{host}\":\"kl\"}}}}\n")).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn wait_builder() -> Result<(), String> {
    let start = Instant::now();
    let mut said = false;
    loop {
        let ok = Command::new("docker")
            .args(["buildx", "inspect", "--bootstrap", BUILDER])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("docker: {e}"))?
            .success();
        if ok {
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
```

- [ ] **Step 9: Run the tests to see them pass**

Run: `deploy/dev/exec.sh cargo test -p kl --lib`
Expected: 9 passed.

- [ ] **Step 10: The binary — `main.rs`**

```rust
//! `kl` — the kloudlite workspace CLI. Two verbs, because a workspace has a builder and a
//! registry and no container engine: `kl build` builds on the owner's builder and pushes,
//! `kl push` copies an image the registry already holds to another name. Nothing here holds a
//! token or calls the api; the docker credential helper (`docker-credential-kl`) is the login.

mod docker;
mod refs;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "kl",
    version,
    about = "kloudlite workspace CLI: build on your builder, push to your registry",
    after_help = "There is no container engine in a workspace: `docker run`, `pull`, `ps` and a separate `docker push` do not apply. A build pushes as it finishes."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build on your builder and push to your registry: `kl build -t hello:1 .`
    Build {
        /// Name[:tag]; `hello:1` means <registry>/<you>/hello:1, `team/hello:1` pushes under the team
        #[arg(short = 't', long = "tag", required = true)]
        tags: Vec<String>,
        #[arg(short = 'f', long)]
        file: Option<String>,
        #[arg(long = "build-arg")]
        build_args: Vec<String>,
        #[arg(long)]
        platform: Option<String>,
        #[arg(long)]
        no_cache: bool,
        #[arg(default_value = ".")]
        context: String,
    },
    /// Copy an image the registry already has to another name: `kl push hello:1 hello:latest`
    Push {
        src: String,
        #[arg(required = false)]
        dst: Vec<String>,
    },
}

const NO_DST: &str = "a build pushes as it finishes — `kl build -t hello:1 .`; `kl push` copies an image the registry already has to another name";

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set — kl runs inside a kloudlite workspace"))
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("kl: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let cli = Cli::parse();
    let (host, owner, buildkit) = (env("KL_REGISTRY_HOST")?, env("KL_OWNER")?, env("BUILDKIT_HOST")?);
    let home = std::env::var("HOME").map(std::path::PathBuf::from).map_err(|_| "HOME is not set".to_string())?;
    docker::ensure_cred_helper(&home, &host)?;
    docker::ensure_builder(&buildkit)?;
    match cli.cmd {
        Cmd::Build { tags, file, build_args, platform, no_cache, context } => {
            let refs: Vec<String> = tags.iter().map(|t| refs::expand(t, &host, &owner)).collect();
            docker::wait_builder()?;
            let meta = std::env::temp_dir().join(format!("kl-build-{}.json", std::process::id()));
            let meta_s = meta.display().to_string();
            let code = docker::run(&docker::build_argv(&refs, file.as_deref(), &build_args, platform.as_deref(), no_cache, &context, &meta_s))?;
            if code != 0 {
                std::process::exit(code);
            }
            // buildx writes `containerimage.digest` into the metadata file; one line per pushed
            // reference is what a script wants to capture.
            let digest = std::fs::read_to_string(&meta)
                .ok()
                .and_then(|m| m.split("\"containerimage.digest\":\"").nth(1).and_then(|r| r.split('"').next()).map(str::to_string));
            let _ = std::fs::remove_file(&meta);
            for r in refs {
                match &digest {
                    Some(d) => println!("{r}@{d}"),
                    None => println!("{r}"),
                }
            }
            Ok(())
        }
        Cmd::Push { src, dst } => {
            if dst.is_empty() {
                return Err(NO_DST.into());
            }
            let src = refs::expand(&src, &host, &owner);
            docker::wait_builder()?;
            for d in dst {
                let d = refs::expand(&d, &host, &owner);
                let code = docker::run(&docker::promote_argv(&src, &d))?;
                if code != 0 {
                    std::process::exit(code);
                }
                println!("{d}");
            }
            Ok(())
        }
    }
}
```

(`imagetools create` runs through the selected builder's buildkit only for the credential lookup; it needs no bootstrap in practice but `wait_builder` is cheap and makes `kl push` after a long idle behave like `kl build` — a cold builder is explained, not a mystery timeout.)

- [ ] **Step 11: Build and lint**

Run: `deploy/dev/exec.sh "cargo build -p kl && cargo clippy -p kl --all-targets -- -D warnings && cargo test -p kl"`
Expected: builds; no warnings; 9 passed. Then a smoke run in the pod:
`deploy/dev/exec.sh "/work/target/debug/kl push hello:1"` → stderr `kl: KL_REGISTRY_HOST is not set — kl runs inside a kloudlite workspace`, exit 1.
`deploy/dev/exec.sh "KL_REGISTRY_HOST=h KL_OWNER=o BUILDKIT_HOST=tcp://x:1 HOME=/tmp/klh /work/target/debug/kl push hello:1"` → exits 1; stderr is either the `NO_DST` sentence or a docker error, depending on whether docker exists in the dev pod — the sentence path is unit-tested by construction (`dst.is_empty()` is checked before any docker call). If it is not, move the `dst.is_empty()` check above `ensure_cred_helper` so the refusal never depends on docker.

- [ ] **Step 12: Commit**

```bash
git add bins/kl Cargo.toml Cargo.lock
git commit -m "Add kl, the workspace CLI: build on the owner's builder, push to the owner's registry"
```

---

### Task 2: Ship `kl` in the workspace image (musl build)

**Files:**
- Modify: `Dockerfile` (`workspace` stage, after the `docker-credential-kl` COPY)
- Modify: `.dockerignore`
- Modify: `deploy/dev/pod/ship.sh` (release build + context staging)
- Modify: `.github/workflows/image.yml` (build step, artifact list, chmod line)
- Modify: `deploy/dev/README.md` (one line: the musl target the pod needs)

**Interfaces:**
- Consumes: the `kl` binary from Task 1.
- Produces: `/usr/local/bin/kl` in `ghcr.io/kloudlite/kloudlite-workspace`.

- [ ] **Step 1: Install the musl target in the dev pod**

Run: `deploy/dev/exec.sh "rustup target add x86_64-unknown-linux-musl && cargo build --release --locked -p kl --target x86_64-unknown-linux-musl && file /work/target/x86_64-unknown-linux-musl/release/kl"`
Expected: `… statically linked …`. (clap is pure Rust; the musl target is self-contained on a glibc host with gcc present, which the pod has.)

- [ ] **Step 2: Dockerfile**

After the `docker-credential-kl` lines in the `workspace` stage add:

```dockerfile
# `kl` is the workspace CLI (build, push). A musl binary because this stage is Alpine: the glibc
# `target/release/*` the other stages copy would not even load here.
COPY target/x86_64-unknown-linux-musl/release/kl /usr/local/bin/kl
RUN chmod 0755 /usr/local/bin/kl
```

- [ ] **Step 3: `.dockerignore`**

Add after the `!target/release/kl-connect` line:

```
!target/x86_64-unknown-linux-musl/release/kl
```

- [ ] **Step 4: `deploy/dev/pod/ship.sh`**

After the `cargo build --release --locked --bins` line add:

```sh
# The workspace CLI, for the Alpine workspace image: its own target, so it never lands in
# target/release beside the glibc binaries.
cargo build --release --locked -p kl --target x86_64-unknown-linux-musl 2>&1 | tail -1
```

After the hardlink loop add:

```sh
mkdir -p "$CTX/target/x86_64-unknown-linux-musl/release"
ln -f /work/target/x86_64-unknown-linux-musl/release/kl "$CTX/target/x86_64-unknown-linux-musl/release/kl"
```

- [ ] **Step 5: `image.yml`**

In the build step's `run:` add a second command after the existing `cargo build --release --locked … --bin kl-connect`:

```yaml
          && rustup target add x86_64-unknown-linux-musl
          && cargo build --release --locked -p kl --target x86_64-unknown-linux-musl
```

(Keep the fold; the step already uses `run: >`.) Add `/ci-target/x86_64-unknown-linux-musl/release/kl` to the `bins` artifact `path:` list. Where the image job restores the artifact, the glibc-ceiling loop must NOT include `kl` (it is musl); leave the loop's list as is. In the image job's `chmod +x` line add `target/x86_64-unknown-linux-musl/release/kl`. Read the download step to confirm the artifact restores to `target/` with the same relative layout — `actions/download-artifact` with `path: target` keeps the `/ci-target/…` suffix only if `merge-multiple` is unset; check what the existing step does for `release/` and mirror it for the musl path.

- [ ] **Step 6: `deploy/dev/README.md`**

Add to the prerequisites list: "`rustup target add x86_64-unknown-linux-musl` once — `ship.sh` builds the workspace CLI for the Alpine image."

- [ ] **Step 7: Verify the image builds and carries the binary**

Run `deploy/dev/pod/ship.sh --no-gate` through pm2 as usual (`PM2_HOME=/work/home/.pm2 /work/npm/bin/pm2 start /work/src/deploy/dev/pod/ship.sh --name ship --no-autorestart -- --no-gate`) after committing and pushing the branch; when it prints `shipped <sha>`, run on the laptop:
`KUBECONFIG=.local/k3s.yaml kubectl run kl-check --rm -i --restart=Never --image=ghcr.io/kloudlite/kloudlite-workspace:<sha> --command -- /usr/local/bin/kl --help`
Expected: the `kl` help text. Delete nothing else; `--rm` removes the pod.

- [ ] **Step 8: Commit**

```bash
git add Dockerfile .dockerignore deploy/dev/pod/ship.sh .github/workflows/image.yml deploy/dev/README.md
git commit -m "Ship kl in the workspace image as a musl binary"
```

---

### Task 3: The probe runs `kl build` and `kl push`

**Files:**
- Modify: `bins/slo/src/stages/workspace.rs` (`build_script`, `build_push`, a new `promote` step)
- Modify: `crates/workspaces/src/slo/catalogue.rs` (`ws.build.p95` SLI text; new `ws.build.promote` row)
- Modify: `deploy/slo.md` (the two rows, held equal by `crates/workspaces/tests/crd_yaml.rs`'s catalogue test — run it to find the exact table shape)
- Modify: `web/apps/web/src/lib/fixtures/superadmin.ts` (the same two rows in the fixture list)

**Interfaces:**
- Consumes: `kl` in the workspace image (Task 2 shipped and pinned on the region before this task's fleet check).
- Produces: SLO ids `ws.build.p95` (script changed) and `ws.build.promote` (new, hourly, stage `5 · Workspace`, target `bound(30_000)`).

- [ ] **Step 1: Write the failing catalogue test change**

In `crates/workspaces/src/slo/catalogue.rs`, change the `ws.build.p95` row's `sli` to:

```
"`kl build` of a two-line Dockerfile in the probe workspace, from a non-login exec, is pushed to the probe owner's own image and its manifest is readable through `/v2`; the builder was Stopped before the step"
```

and add directly after it:

```rust
    // `kl push` is a registry-side copy through buildx imagetools; the probe promotes the image
    // the build above just pushed and reads the new tag's digest back, so the step proves both
    // the copy and that the credential helper serves imagetools as it serves build.
    Slo { id: "ws.build.promote", feature: "Workspaces", sli: "`kl push` copies the probe's just-built image to a second tag and `docker buildx imagetools inspect` reads that tag's digest back", target: bound(30_000), suite: Suite::Hourly, stage: "5 · Workspace" },
```

- [ ] **Step 2: Run the catalogue test to see it fail**

Run: `deploy/dev/exec.sh cargo test -p kloudlite-workspaces --test crd_yaml`
Expected: FAIL — `deploy/slo.md` no longer matches; the failure output prints the expected table.

- [ ] **Step 3: Update `deploy/slo.md` and the web fixture**

Edit the `ws.build.p95` row's SLI to the new text and add the `ws.build.promote` row after it, in the same column format the test prints:

```
| `ws.build.promote` | Workspaces | `kl push` copies the probe's just-built image to a second tag and `docker buildx imagetools inspect` reads that tag's digest back | 99.9 % ≤ 30000 ms | hourly | 5 · Workspace |
```

Mirror both rows in `web/apps/web/src/lib/fixtures/superadmin.ts` (the array of `[id, feature, sli, target, suite, stage]` tuples around line 428–600).

- [ ] **Step 4: Run the catalogue test to see it pass**

Run: `deploy/dev/exec.sh cargo test -p kloudlite-workspaces --test crd_yaml`
Expected: PASS.

- [ ] **Step 5: Change the build script and add the promote step**

In `bins/slo/src/stages/workspace.rs` replace `build_script` with:

```rust
/// The script the step runs, in the workspace, as the person — `kl build` from a NON-login exec,
/// with none of `kl-build.sh`'s setup: that `kl` creates its own builder and credential config
/// is the clause the spec calls self-sufficiency, and this is where it is held.
fn build_script(run_id: &str) -> String {
    format!(
        "mkdir -p /tmp/d\n\
printf 'FROM alpine:3.20\\nRUN echo slo > /slo\\n' > /tmp/d/Dockerfile\n\
kl build -t slo-build:{run_id} /tmp/d"
    )
}

/// `kl push` from the same workspace, then the promoted tag's digest read back through buildx's
/// own imagetools so the copy is verified by a second, independent reader.
fn promote_script(run_id: &str) -> String {
    format!(
        "kl push slo-build:{run_id} slo-build:{run_id}-promoted\n\
docker buildx imagetools inspect $KL_REGISTRY_HOST/$KL_OWNER/slo-build:{run_id}-promoted --format '{{{{json .Manifest.Digest}}}}'"
    )
}
```

Update `build_push`'s call to `build_script(&run_id)` (drop the `probe`/`registry` arguments from the script but KEEP them for the `/v2` manifest read-back that follows). Update the doc comment above `build_push` (the paragraph about `kl-build.sh`) to say the script deliberately does NOT source it.

Add after `build_push` in the stage flow (line ~100, right after `build_push(c, &id).await;`) a call `promote(c, &id).await;` and the function:

```rust
/// `ws.build.promote`, hourly only, right after the build so the source tag exists. Skipped —
/// never failed — when the build step itself did not pass, since a missing source says nothing
/// about `kl push`.
async fn promote(c: &mut Ctx, id: &str) {
    if c.suite != Suite::Hourly {
        return;
    }
    if !c.passed("ws.build.p95") {
        return c.skip("ws.build.promote", "the build step did not pass");
    }
    let (run_id, id) = (c.run_id.clone(), id.to_string());
    c.step("ws.build.promote", PROMOTE_CEILING, move |c| {
        let (run_id, id) = (run_id.clone(), id.clone());
        async move {
            let (code, out, err) = ws_exec(c, &id, &promote_script(&run_id), PROMOTE_CEILING).await?;
            if code != 0 {
                return Err(anyhow!("kl push failed ({code}): {} {}", out.trim(), err.trim()));
            }
            if !out.contains("\"sha256:") {
                return Err(anyhow!("the promoted tag's digest did not read back: {}", out.trim()));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}
```

with `const PROMOTE_CEILING: Duration = Duration::from_secs(30);` beside `BUILD_CEILING`. If `Ctx` has no `passed(id)` helper, add one that reads the run's recorded step results (look at how `c.skip` and `c.step` record outcomes — `Ctx.results` or similar — and return whether that id was recorded `ok: true`); a one-line method, unit-tested only through the existing step machinery.

- [ ] **Step 6: Unit tests for the scripts**

In `workspace.rs`'s `tests` module add:

```rust
    #[test]
    fn the_build_script_runs_kl_build_without_sourcing_the_login_setup() {
        let s = build_script("hourly-1");
        assert!(s.contains("kl build -t slo-build:hourly-1 /tmp/d"), "{s}");
        assert!(!s.contains("kl-build.sh"), "{s}");
    }

    #[test]
    fn the_promote_script_reads_the_new_tag_back_through_imagetools() {
        let s = promote_script("hourly-1");
        assert!(s.contains("kl push slo-build:hourly-1 slo-build:hourly-1-promoted"), "{s}");
        assert!(s.contains("imagetools inspect"), "{s}");
    }
```

Run: `deploy/dev/exec.sh "cargo test -p kloudlite-slo-bin && cargo clippy -p kloudlite-slo-bin --all-targets -- -D warnings"`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add bins/slo/src/stages/workspace.rs crates/workspaces/src/slo/catalogue.rs deploy/slo.md web/apps/web/src/lib/fixtures/superadmin.ts
git commit -m "Probe: ws.build.p95 runs kl build from a non-login exec, ws.build.promote covers kl push"
```

- [ ] **Step 8: Fleet check**

Ship (full gate), pin, roll AKS and region, run `deploy/dev/run-job.sh hourly`, then read BOTH ids by result:

```sql
SELECT Timestamp, Body, LogAttributes['slo_id'], LogAttributes['ok'], LogAttributes['ms'], LogAttributes['detail']
FROM default.otel_logs WHERE Body IN ('slo.step.done','slo.step.skipped')
  AND LogAttributes['slo_id'] IN ('ws.build.p95','ws.build.promote') AND Timestamp > now() - INTERVAL 30 MINUTE
```

Expected: two `slo.step.done` rows, `ok=true`. A `slo.step.skipped` row is NOT a pass. Record the run id and both `ms` values in the ledger.

---

### Task 4: Documentation

**Files:**
- Modify: `CLAUDE.md` (the "Image builds run on a hidden per-owner builder…" paragraph)
- Modify: `deploy/workspace-image/kl-build.sh` (comment only)

- [ ] **Step 1: The project guide**

In the builds paragraph of `CLAUDE.md`, after the sentence ending "`docker buildx build --push` never sees a raw token on the command line;", add:

```
The tool a person runs is `kl` (`bins/kl`, a musl binary in the workspace image, `clap` only):
`kl build -t hello:1 .` builds on the builder and pushes as `{registry}/{owner}/hello:1`, and
`kl push hello:1 hello:latest` copies an image the registry already holds — a registry-side
`buildx imagetools create`, because there is no local image store for a push to read from.
`kl` makes its own buildx builder and credential config, so it works from any exec, not only a
login shell; `kl-connect` (`bins/kl-connect`) is the laptop CLI and shares nothing with it.
```

- [ ] **Step 2: `kl-build.sh` comment**

Change the first comment line to: `# Builds go to the owner's builder through the gate; the credential helper is the login. \`kl\` does this setup itself; this is for people who call \`docker buildx\` directly.`

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md deploy/workspace-image/kl-build.sh
git commit -m "Document kl, the workspace CLI, beside kl-connect"
```

---

## Self-review

- **Spec coverage.** §2 verbs → Task 1; §3 expansion, env, self-sufficiency, output, wait → Task 1 (Steps 4, 8, 10); §4 promote and refusal → Task 1; §5 packaging → Task 2; §6 non-goals need no task; §7 unit → Task 1 Steps 2/6, fleet → Task 3 Step 8; rename precondition already committed (`f5663b1c`).
- **Placeholders.** None: every step has its code or exact command. Task 2 Step 5 asks the implementer to read the download step rather than guess its layout — that is an instruction, not a TBD.
- **Type consistency.** `refs::expand(&str,&str,&str)->String` used identically in Task 1 Step 10; `docker::build_argv` signature matches its test and its call; `promote_argv(src,dst)` order matches the test (`-t dst src`); `bound(30_000)` is the catalogue's existing constructor; `PROMOTE_CEILING` defined beside `BUILD_CEILING`.

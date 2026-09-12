//! `kl` — the kloudlite workspace CLI. Two build verbs, because a workspace has a builder and a
//! registry and no container engine: `kl build` builds on the owner's builder and pushes,
//! `kl push` copies an image the registry already holds to another name. Nothing here holds a
//! token or calls the api; the docker credential helper (`docker-credential-kl`) is the login.
//! The third verb is `kl ide serve`, the workspace tool server (`kloudlite_ide`), started by the
//! pod prelude before sshd and reached through the ssh tunnel.

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
        dst: Vec<String>,
    },
    /// The workspace tool server: files, exec, watch and graft over a plain HTTP tool API on loopback
    Ide {
        #[command(subcommand)]
        cmd: IdeCmd,
    },
}

#[derive(Subcommand)]
enum IdeCmd {
    /// Serve on 127.0.0.1:7788; reach it through `kl-connect ws ide <workspace>`
    Serve {
        #[arg(long, default_value = "127.0.0.1:7788")]
        bind: std::net::SocketAddr,
        /// A graft context directory other than `$KL_WORKSPACE/graft`
        #[arg(long)]
        graft_dir: Option<std::path::PathBuf>,
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
    // The server needs none of the docker setup below and must not wait on the builder.
    if let Cmd::Ide { cmd: IdeCmd::Serve { bind, graft_dir } } = cli.cmd {
        return serve_ide(bind, graft_dir);
    }
    // Refused before anything touches docker: the sentence is the whole point of the verb.
    if let Cmd::Push { dst, .. } = &cli.cmd {
        if dst.is_empty() {
            return Err(NO_DST.into());
        }
    }
    let (host, owner, buildkit) = (env("KL_REGISTRY_HOST")?, env("KL_OWNER")?, env("BUILDKIT_HOST")?);
    let home = std::env::var("HOME").map(std::path::PathBuf::from).map_err(|_| "HOME is not set".to_string())?;
    docker::ensure_cred_helper(&home, &host)?;
    docker::ensure_builder(&buildkit)?;
    docker::wait_builder()?;
    match cli.cmd {
        Cmd::Build { tags, file, build_args, platform, no_cache, context } => {
            let refs: Vec<String> = tags.iter().map(|t| refs::expand(t, &host, &owner)).collect();
            // `NamedTempFile`, not a path built from the pid: `/tmp` is shared, the old name was
            // guessable, and buildx follows a symlink planted at it — so another user on the same
            // machine could choose where the build's metadata was written (2026-09-12). Created
            // 0600 by this process and removed when it drops.
            let meta = tempfile::Builder::new()
                .prefix("kl-build-")
                .suffix(".json")
                .tempfile()
                .map_err(|e| format!("could not create the build metadata file: {e}"))?;
            let meta_s = meta.path().display().to_string();
            let code = docker::run(&docker::build_argv(&refs, file.as_deref(), &build_args, platform.as_deref(), no_cache, &context, &meta_s))?;
            if code != 0 {
                std::process::exit(code);
            }
            // buildx writes `containerimage.digest` into the metadata file; one line per pushed
            // reference is what a script wants to capture.
            let digest = std::fs::read_to_string(meta.path())
                .ok()
                .and_then(|m| m.split("\"containerimage.digest\":\"").nth(1).and_then(|r| r.split('"').next()).map(str::to_string));
            for r in refs {
                match &digest {
                    Some(d) => println!("{r}@{d}"),
                    None => println!("{r}"),
                }
            }
            Ok(())
        }
        // Returned before the docker setup above; the match still has to say so.
        Cmd::Ide { .. } => unreachable!("kl ide serve is handled before the docker setup"),
        Cmd::Push { src, dst } => {
            let src = refs::expand(&src, &host, &owner);
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

/// `kl ide serve`: resolve the pod's environment, refuse to start without the preconditions,
/// then serve until killed. Logs are JSON on stderr, like every other binary's.
fn serve_ide(bind: std::net::SocketAddr, graft_dir: Option<std::path::PathBuf>) -> Result<(), String> {
    let root = std::path::PathBuf::from(env("KL_WORKSPACE")?);
    let home = std::path::PathBuf::from(env("HOME")?);
    let cfg = kloudlite_ide::Config { bind, root, home, graft_dir };
    kloudlite_ide::guard::preflight(&cfg)?;
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(kloudlite_ide::serve(cfg)).map_err(|e| e.to_string())
}

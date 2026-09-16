//! `kl` — the kloudlite workspace CLI, from inside the workspace.
//!
//! `kl container build|push`, because a workspace has a builder and a registry and no container
//! engine: a build runs on the owner's builder and pushes as it finishes, and `push` copies an
//! image the registry already holds to another name. Those two hold no api credential; the docker
//! credential helper (`docker-credential-kl`) is their login.
//!
//! `kl pkg` and `kl env` DO call `/v1`, with the `workspace-token` the keys beat projects into the
//! pod (`api.rs`). `kl ide serve` is the workspace tool server, started by the pod prelude — a
//! hidden verb, because no person types it.

mod api;
mod docker;
mod env;
mod pkg;
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
    /// Container images: build on your builder, or copy one the registry already has
    Container {
        #[command(subcommand)]
        cmd: ContainerCmd,
    },
    /// This workspace's packages
    Pkg {
        #[command(subcommand)]
        cmd: PkgCmd,
    },
    /// The environment your workspaces in this team follow
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// The workspace tool server: files, exec, watch and graft over a plain HTTP tool API
    #[command(hide = true)]
    Ide {
        #[command(subcommand)]
        cmd: IdeCmd,
    },
}

#[derive(Subcommand)]
enum ContainerCmd {
    /// Build on your builder and push to your registry: `kl container build -t hello:1 .`
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
    /// Copy an image the registry already has to another name: `kl container push hello:1 hello:latest`
    Push {
        src: String,
        dst: Vec<String>,
    },
}

#[derive(Subcommand)]
enum PkgCmd {
    /// The declared packages with the versions they are locked at
    List,
    /// Declare packages: `kl pkg add jq nodejs@20` (attr or attr@version)
    Add {
        #[arg(required = true)]
        entries: Vec<String>,
    },
    /// Undeclare packages by name; a version suffix on the argument is ignored
    Rm {
        #[arg(required = true)]
        entries: Vec<String>,
    },
    /// Re-resolve every pinned entry against the index
    Update,
}

#[derive(Subcommand)]
enum EnvCmd {
    /// The team's environments, the one this space follows marked
    List,
    /// The environment this space follows, or `none`
    Current,
    /// Follow an environment, by name or id
    Switch { target: String },
    /// Follow no environment
    Clear,
}

#[derive(Subcommand)]
enum IdeCmd {
    /// Serve on 0.0.0.0:7788: a person's bench reaches it inside the namespace, `kl-connect ws ide` over the tunnel
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
    match Cli::parse().cmd {
        // The server needs none of the docker setup and must not wait on the builder.
        Cmd::Ide { cmd: IdeCmd::Serve { bind, graft_dir } } => serve_ide(bind, graft_dir),
        Cmd::Container { cmd } => container(cmd),
        Cmd::Pkg { cmd } => {
            let (api, id) = (api::Api::from_env()?, env("KL_WORKSPACE_ID")?);
            match cmd {
                PkgCmd::List => pkg::list(&api, &id),
                PkgCmd::Add { entries } => pkg::add(&api, &id, &entries),
                PkgCmd::Rm { entries } => pkg::rm(&api, &id, &entries),
                PkgCmd::Update => pkg::update(&api, &id),
            }
        }
        Cmd::Env { cmd } => {
            let (api, team) = (api::Api::from_env()?, env("KL_TEAM")?);
            match cmd {
                EnvCmd::List => env::list(&api, &team),
                EnvCmd::Current => env::current(&api, &team),
                EnvCmd::Switch { target } => env::switch(&api, &team, &target),
                EnvCmd::Clear => env::clear(&api, &team),
            }
        }
    }
}

/// The two verbs that drive `docker buildx`: the credential helper and the builder are set up
/// here and nowhere else, so `kl pkg`/`kl env` never wait on a builder that is starting.
fn container(cmd: ContainerCmd) -> Result<(), String> {
    // Refused before anything touches docker: the sentence is the whole point of the verb.
    if let ContainerCmd::Push { dst, .. } = &cmd {
        if dst.is_empty() {
            return Err(NO_DST.into());
        }
    }
    let (host, owner, buildkit) = (env("KL_REGISTRY_HOST")?, env("KL_OWNER")?, env("BUILDKIT_HOST")?);
    let home = std::env::var("HOME").map(std::path::PathBuf::from).map_err(|_| "HOME is not set".to_string())?;
    docker::ensure_cred_helper(&home, &host)?;
    docker::ensure_builder(&buildkit)?;
    docker::wait_builder()?;
    match cmd {
        ContainerCmd::Build { tags, file, build_args, platform, no_cache, context } => {
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
        ContainerCmd::Push { src, dst } => {
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
    let cfg = kloudlite_ide::Config { bind, root: root.clone(), home, graft_dir };
    kloudlite_ide::guard::preflight(&cfg)?;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    // Before the runtime: the exporter's blocking client must not be built inside one.
    // `None` (no `KLOUDLITE_OTLP_URL`) is exactly the old subscriber.
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .with(kloudlite_trace::layer())
        .with(tracing_subscriber::fmt::layer().json().with_writer(std::io::stderr))
        .init();
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    // Before the first socket: tmux-resurrect brings back the terminal names, layout and pane
    // text a stop, a move or a clone left in `{ws}/.cache/tmux`, so a reconnect finds them.
    rt.block_on(async {
        kloudlite_ide::pty::restore(&root).await;
        kloudlite_ide::serve(cfg).await
    })
    .map_err(|e| e.to_string())
}


#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn the_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_lists_the_three_verbs_and_hides_ide() {
        let help = Cli::command().render_help().to_string();
        for v in ["container", "pkg", "env"] {
            assert!(help.contains(v), "{v} missing from:\n{help}");
        }
        assert!(!help.contains("ide"), "ide should be hidden:\n{help}");
    }

    #[test]
    fn the_old_spellings_are_gone_and_ide_still_parses() {
        assert!(Cli::try_parse_from(["kl", "build", "-t", "x:1"]).is_err());
        assert!(Cli::try_parse_from(["kl", "container", "build", "-t", "x:1"]).is_ok());
        assert!(Cli::try_parse_from(["kl", "ide", "serve"]).is_ok());
        // Every editing verb needs something to edit.
        assert!(Cli::try_parse_from(["kl", "pkg", "add"]).is_err());
    }
}

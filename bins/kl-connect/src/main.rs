//! `kl-connect` — the kloudlite laptop CLI: log in once, then ssh into a workspace through the region gateway.
//!
//! Hidden env vars, for tests and the e2e script only:
//!   KL_CONFIG_DIR       where config.json and known_hosts live (default ~/.config/kl-connect)
//!   KL_GATEWAY_OVERRIDE replaces the origin of the api-supplied gateway URL

mod api;
mod builder;
mod config;
mod login;
mod proxy;
mod sshconfig;
mod ws;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "kl-connect",
    version,
    about = "kloudlite connect CLI: log in, list workspaces, ssh into one",
    after_help = "Hidden, for tests and e2e only:\n  \
        KL_CONFIG_DIR        where config.json and known_hosts live (default ~/.config/kl-connect)\n  \
        KL_GATEWAY_OVERRIDE  replaces the origin of the api-supplied gateway URL"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in through the browser and store the CLI token
    Login {
        #[arg(long, default_value = config::DEFAULT_API)]
        api: String,
    },
    /// Revoke this machine's CLI token and forget it
    Logout,
    /// Workspaces
    Ws {
        #[command(subcommand)]
        cmd: WsCmd,
    },
    /// The hidden per-owner build environment
    Builder {
        #[command(subcommand)]
        cmd: BuilderCmd,
    },
}

#[derive(Subcommand)]
enum BuilderCmd {
    /// State, readiness, and why it isn't ready yet
    Status {
        #[arg(long)]
        team: Option<String>,
    },
}

#[derive(Subcommand)]
enum WsCmd {
    /// List your workspaces
    List {
        #[arg(long)]
        team: Option<String>,
    },
    /// ssh into a workspace: `kl-connect ws ssh gh -- -A`
    Ssh {
        target: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// ssh's ProxyCommand: pump stdio to the workspace's gateway tunnel
    Proxy { id: String },
    /// Write ~/.ssh/kloudlite_config and Include it from ~/.ssh/config
    SshConfig,
    /// Tunnel the workspace's tool API to localhost: `kl-connect ws ide api`, then
    /// `curl http://localhost:7788/tools`
    Ide {
        target: String,
        #[arg(long, default_value_t = 7788)]
        port: u16,
    },
}

#[tokio::main]
async fn main() {
    // Two TLS clients in one binary (reqwest and tungstenite) and two providers reachable in the
    // graph: rustls will not pick one on its own.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    let r = match &cli.cmd {
        Cmd::Login { api } => login::login(api.clone()).await,
        Cmd::Logout => login::logout().await,
        Cmd::Ws { cmd } => match cmd {
            WsCmd::List { team } => ws::list(team.as_deref()).await,
            WsCmd::Ssh { target, args } => ws::ssh(target, args).await,
            WsCmd::Proxy { id } => proxy::proxy(id).await,
            WsCmd::SshConfig => ws::ssh_config().await,
            WsCmd::Ide { target, port } => ws::ide(target, *port).await,
        },
        Cmd::Builder { cmd } => match cmd {
            BuilderCmd::Status { team } => builder::status(team.as_deref()).await,
        },
    };
    if let Err(e) = r {
        eprintln!("kl-connect: {e}");
        std::process::exit(1);
    }
}

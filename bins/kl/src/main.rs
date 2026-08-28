//! `kl` — the kloudlite CLI: log in once, then ssh into a workspace through the region gateway.
//!
//! Hidden env vars, for tests and the e2e script only:
//!   KL_CONFIG_DIR       where config.json and known_hosts live (default ~/.config/kl)
//!   KL_GATEWAY_OVERRIDE replaces the origin of the api-supplied gateway URL

mod api;
mod config;
mod login;
mod proxy;
mod sshconfig;
mod ws;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "kl",
    version,
    about = "kloudlite CLI",
    after_help = "Hidden, for tests and e2e only:\n  \
        KL_CONFIG_DIR        where config.json and known_hosts live (default ~/.config/kl)\n  \
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
}

#[derive(Subcommand)]
enum WsCmd {
    /// List your workspaces
    List {
        #[arg(long)]
        team: Option<String>,
    },
    /// ssh into a workspace: `kl ws ssh gh -- -A`
    Ssh {
        target: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// ssh's ProxyCommand: pump stdio to the workspace's gateway tunnel
    Proxy { id: String },
    /// Write ~/.ssh/kloudlite_config and Include it from ~/.ssh/config
    SshConfig,
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
        },
    };
    if let Err(e) = r {
        eprintln!("kl: {e}");
        std::process::exit(1);
    }
}

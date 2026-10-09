//! `kl-connect` — the kloudlite laptop CLI: log in once, then ssh into a bench (`kl-connect [team]`)
//! or a workspace through the region gateway.
//!
//! Hidden env vars, for tests and the e2e script only:
//!   KL_CONFIG_DIR       where config.json and known_hosts live (default ~/.config/kl-connect)
//!   KL_GATEWAY_OVERRIDE replaces the origin of the api-supplied gateway URL

mod api;
mod bench;
mod builder;
mod clip;
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
    about = "kloudlite connect CLI: `kl-connect` opens your bench, `kl-connect <team>` the team's",
    // A team named like a subcommand (`login`, `ws`, ...) is read as the subcommand.
    args_conflicts_with_subcommands = true,
    after_help = "Hidden, for tests and e2e only:\n  \
        KL_CONFIG_DIR        where config.json and known_hosts live (default ~/.config/kl-connect)\n  \
        KL_GATEWAY_OVERRIDE  replaces the origin of the api-supplied gateway URL"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// The team whose bench to open; your own bench when absent
    team: Option<String>,
    /// Start the bench first (creating it if it does not exist)
    #[arg(long)]
    start: bool,
    /// Only used for an unbound personal bench's first `--start` (a team's region is the team's).
    #[arg(long)]
    region: Option<String>,
    /// Run the TUI on the bench instead of on this laptop (slower to type in; for when kl-tui misbehaves).
    #[arg(long)]
    remote_tui: bool,
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
    /// Claude Code on the bench
    Claude {
        #[command(subcommand)]
        cmd: ClaudeCmd,
    },
    /// ssh's ProxyCommand for `kl-connect [team]`: pump stdio to the bench's gateway tunnel
    #[command(hide = true)]
    BenchProxy {
        /// Pump to the bench daemon's TUI port instead of sshd (the laptop kl-tui's direct path)
        #[arg(long)]
        tui: bool,
        team: Option<String>,
    },
}

#[derive(Subcommand)]
enum ClaudeCmd {
    /// Sign this bench's Claude Code in to your Anthropic account
    Login {
        #[arg(long)]
        team: Option<String>,
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
        None => bench::bench(cli.team.as_deref(), cli.start, cli.region.as_deref(), cli.remote_tui).await,
        Some(cmd) => run(cmd).await,
    };
    if let Err(e) = r {
        eprintln!("kl-connect: {e}");
        std::process::exit(1);
    }
}

async fn run(cmd: &Cmd) -> Result<(), String> {
    match cmd {
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
        Cmd::Claude { cmd: ClaudeCmd::Login { team } } => bench::claude_login(team.as_deref()).await,
        Cmd::BenchProxy { team, tui } => bench::proxy(team.as_deref(), *tui).await,
    }
}

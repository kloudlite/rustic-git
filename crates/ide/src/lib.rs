//! `kl ide serve`: the tool server inside a workspace pod.
//!
//! A session running OUTSIDE the workspace — Claude Code on a laptop, a CI agent, the console —
//! needs the same handful of tools it uses locally, executed inside the pod against the tree and
//! the toolchain that live there. This crate is those tools, typed and bounded, behind a plain
//! HTTP tool API on port 7788 of the pod IP (`GET /tools`, `POST /tools/{name}`): `read`, `write`,
//! `edit`, `glob`, `grep`, `exec` (a job, or a detached process with an id), `process_*`, `watch*`,
//! and graft's own tools proxied from a `graft mcp` child. The namespace is the fence: the owner's
//! bench reaches it there (`allow-bench-tools`), `kl-connect ws ide` over the ssh tunnel, so there
//! is no second credential and no auth code here. MCP is deliberately NOT spoken here: the
//! session layer above speaks it once for every workspace a person holds and forwards to this API.
//!
//! Design: `docs/superpowers/specs/2026-09-11-kl-ide-serve-design.md`. Module map: `guard`
//! (the preconditions the server refuses to start without), `server` (axum routes), `api`
//! (the tool routes), `trees` (the tree a call acts on and the map of them), `sandbox` (the
//! bubblewrap argv every exec runs under), `fs/` (the workspace-state routes a UI renders from: tree, stat,
//! file, git, changes, diff — read-only, conditional, not tools), `paths` (confinement to the home), `tools/` (one file per tool
//! family), `procs` (detached processes and their ring buffers), `stream` (the two WebSocket
//! streams), `graft` (the child and the freshness triggers).
pub mod api;
pub mod auth;
pub mod fs;
pub mod graft;
pub mod guard;
pub mod paths;
pub mod sandbox;
pub mod trees;
pub mod procs;
pub mod server;
pub mod stream;
pub mod tools;
pub mod watches;

use std::net::SocketAddr;
use std::path::PathBuf;

/// What `kl ide serve` resolves from its flags and the pod's environment.
#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    /// The workspace directory (`$KL_WORKSPACE`), which is the MAIN tree's root. Every other
    /// tree is `{root}/.agents/{name}`; a call names one with `tree` and every path in and out of
    /// it is relative to that tree (spec §3.5).
    pub root: PathBuf,
    /// `$HOME`. NOT a confinement boundary any more — it was, so a model could edit dotfiles, and
    /// that is the person's shell's job now. Kept because the server still resolves the nix
    /// profile under it for the sandbox.
    pub home: PathBuf,
    /// A graft context directory other than `{root}/graft`.
    pub graft_dir: Option<PathBuf>,
    /// The workspace token every request must carry, as a FILE — read per request, because the
    /// keys beat re-mints it and a value cached at boot would refuse its own callers an hour in.
    /// `None` is the default path (`auth::TOKEN_PATH`); a test points it at a tempdir.
    pub token_path: Option<PathBuf>,
}

pub use server::serve;
pub use trees::{TreeCtx, Trees};

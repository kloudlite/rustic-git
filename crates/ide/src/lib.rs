//! `kl ide serve`: the tool server inside a workspace pod.
//!
//! A session running OUTSIDE the workspace — Claude Code on a laptop, a CI agent, the console —
//! needs the same handful of tools it uses locally, executed inside the pod against the tree and
//! the toolchain that live there. This crate is those tools, typed and bounded, behind a plain
//! HTTP tool API on `127.0.0.1:7788` (`GET /tools`, `POST /tools/{name}`): `read`, `write`, `edit`, `glob`, `grep`, `exec` (a job, or a
//! detached process with an id), `process_*`, `watch*`, and graft's own tools proxied from a
//! `graft mcp` child. Loopback only: the ssh tunnel a person already holds is the boundary, so
//! there is no second credential and no auth code here. MCP is deliberately NOT spoken here: the
//! session layer above speaks it once for every workspace a person holds and forwards to this API.
//!
//! Design: `docs/superpowers/specs/2026-09-11-kl-ide-serve-design.md`. Module map: `guard`
//! (the preconditions the server refuses to start without), `server` (axum routes), `api`
//! (the tool routes), `paths` (confinement to the home), `tools/` (one file per tool
//! family), `procs` (detached processes and their ring buffers), `stream` (the two WebSocket
//! streams), `graft` (the child and the freshness triggers).
pub mod api;
pub mod graft;
pub mod guard;
pub mod paths;
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
    /// The workspace directory (`$KL_WORKSPACE`), the tree every relative path resolves against.
    pub root: PathBuf,
    /// `$HOME`; every path a tool touches must stay under it.
    pub home: PathBuf,
    /// A graft context directory other than `{root}/graft`.
    pub graft_dir: Option<PathBuf>,
}

pub use server::serve;

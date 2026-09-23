//! The tree a call acts on, and the resolver that hands it over.
//!
//! One `kl ide serve` serves every tree of its workspace: the main working directory and each
//! `~/.agents/{name}` the node agent cut for a subagent (a snapshot of the whole home, so the tree's
//! own copy of the workspace is `~/.agents/{name}/workspace`; `Trees::dir_of`). Every tool argument and every `/fs/*`
//! query takes an optional `tree` (absent or `main` means the workspace itself), and the server
//! confines the call to that tree's root before it runs.
//!
//! The server does NOT know which session called. It trusts the bench for that — the bench pins
//! `tree` per session — and confines for itself; the two together are the boundary, and neither
//! alone would be one. That split is deliberate: the pod's token authorises the whole pod, so a
//! confinement that depended on identity would have nothing to key on.

use crate::graft::Graft;
use crate::tools::ToolError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// The directory nested tree subvolumes live in, under the top of the worktree (the home). The same name the node
/// agent writes and the global gitignore carries.
pub const TREES_DIR: &str = ".agents";

/// The main tree's name in a request. Spelled rather than left implicit so a caller that pins
/// `tree` on every call — the bench does — has something to pin it to.
pub const MAIN: &str = "main";

/// The first tree's port block. Each tree gets a hundred ports from here, by creation order;
/// `main` gets none, because it IS the workspace and the ports a person already uses are not to be
/// moved out from under them.
pub const PORT_BASE: u16 = 20_000;
pub const PORT_BLOCK: u16 = 100;

/// The charset `/v1` writes a tree name in. Checked again here — this crate cannot depend on
/// `kloudlite-workspaces` (the dependency runs the other way), and a name arrives as a path
/// segment: `.`, `..` and `/` are all traversal wearing a name, whatever wrote it.
fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}


/// One tree, resolved. `root` is what every path in and out of a call is relative to.
// `Debug` by hand: `Graft` holds a child process and a notifier, neither of which prints, and a
// test that unwraps a resolve wants the name and the root, not either of those.
pub struct TreeCtx {
    pub name: String,
    pub root: PathBuf,
    /// One graph per tree: a subagent's graft answers about ITS files, and the main session's
    /// about the workspace's.
    pub graft: Arc<Graft>,
    /// `None` for `main`. A tree's block is `20000 + 100*i` for the i-th tree by creation order,
    /// handed to every exec as `PORT` and `KL_PORT_RANGE` (spec §4.6).
    pub port_block: Option<(u16, u16)>,
}

impl std::fmt::Debug for TreeCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeCtx").field("name", &self.name).field("root", &self.root).field("port_block", &self.port_block).finish()
    }
}

impl TreeCtx {
    /// Whether this tree is the workspace itself, which is the one tree with a place it may not
    /// look (`.agents/`) and the one with no port block.
    pub fn is_main(&self) -> bool {
        self.name == MAIN
    }

    /// The per-tree dotfile home `bwrap` sets `HOME` to, so a subagent's git, npm and cargo config
    /// lands in its own tree rather than in the person's home.
    pub fn sandbox_home(&self) -> PathBuf {
        self.root.join(".home")
    }
}


/// The lazily-built map of served trees. A tree is created on first use when its directory
/// exists, and dropped when the directory is gone — checked on every use, because the node agent
/// deletes a tree under us the moment `/v1` says to.
pub struct Trees {
    /// The workspace directory: `main`'s root.
    root: PathBuf,
    /// Where `.agents/` sits: the top of the btrfs worktree, which is the home since the home
    /// became the workspace volume (ruling 2026-09-22). The agent snapshots the WHOLE worktree
    /// into `{top}/.agents/{name}`, so a tree's copy of `root` is `root`'s path below `top`,
    /// replayed under the tree. Merged without this, the server looked for
    /// `~/workspace/.agents/{name}` while the agent cut `~/.agents/{name}` (hourly 2026-09-23
    /// 16:58 IST, `ws.tree.cut`: "no such tree").
    top: PathBuf,
    graft_dir: Option<PathBuf>,
    map: RwLock<HashMap<String, Arc<TreeCtx>>>,
    /// Creation order, which is what a port block is keyed by. Kept beside the map rather than
    /// derived from it: a `HashMap`'s order is not one, and a tree that is dropped and re-created
    /// must not silently take another's ports.
    order: RwLock<Vec<String>>,
}

impl Trees {
    /// Trees under `{root}/.agents`: the worktree IS the workspace directory.
    pub fn new(root: PathBuf, graft_dir: Option<PathBuf>) -> Self {
        Self::under(root.clone(), root, graft_dir)
    }

    /// Trees under `{top}/.agents`, each holding its copy of `root` at the same relative path.
    /// A `root` outside `top` falls back to `new`'s layout rather than inventing one.
    pub fn under(top: PathBuf, root: PathBuf, graft_dir: Option<PathBuf>) -> Self {
        let top = if root.starts_with(&top) { top } else { root.clone() };
        Trees { root, top, graft_dir, map: RwLock::new(HashMap::new()), order: RwLock::new(Vec::new()) }
    }

    /// The directory a tree's files live in.
    pub fn dir_of(&self, name: &str) -> PathBuf {
        if name == MAIN {
            return self.root.clone();
        }
        let dir = self.top.join(TREES_DIR).join(name);
        // `join("")` would append a trailing separator, and the path is compared and bound verbatim.
        match self.root.strip_prefix(&self.top) {
            Ok(rel) if !rel.as_os_str().is_empty() => dir.join(rel),
            _ => dir,
        }
    }

    /// Resolve a request's `tree` argument. `None` and `main` are the workspace; anything else
    /// must be a directory under `.agents/` that exists RIGHT NOW.
    ///
    /// An unknown tree is `Invalid` (400), not `Denied`: there is nothing to refuse the caller —
    /// the name simply does not name a tree, and the bench that pinned it needs to hear that.
    pub fn resolve(&self, name: Option<&str>) -> Result<Arc<TreeCtx>, ToolError> {
        let name = match name {
            None | Some("") | Some(MAIN) => MAIN,
            Some(n) => n,
        };
        if name != MAIN {
            // Checked before the map, not after: a cached ctx for a tree the agent has since
            // deleted would keep serving a root that is gone.
            if !name_ok(name) {
                return Err(ToolError::Invalid(format!("{name}: not a tree name")));
            }
            if !self.dir_of(name).is_dir() {
                self.forget(name);
                return Err(ToolError::Invalid(format!("{name}: no such tree")));
            }
        }
        if let Some(t) = self.map.read().unwrap_or_else(|p| p.into_inner()).get(name) {
            return Ok(t.clone());
        }
        Ok(self.create(name))
    }

    /// Every tree this server has built a ctx for, `main` included. What `/healthz` reports a
    /// graph state per; a tree nobody has asked about yet has no graph to report.
    pub fn served(&self) -> Vec<Arc<TreeCtx>> {
        let mut v: Vec<Arc<TreeCtx>> = self.map.read().unwrap_or_else(|p| p.into_inner()).values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn forget(&self, name: &str) {
        self.map.write().unwrap_or_else(|p| p.into_inner()).remove(name);
    }

    fn create(&self, name: &str) -> Arc<TreeCtx> {
        let mut map = self.map.write().unwrap_or_else(|p| p.into_inner());
        // Re-checked under the write lock: two first calls for the same tree would otherwise
        // build two graphs and two port blocks for it.
        if let Some(t) = map.get(name) {
            return t.clone();
        }
        let root = self.dir_of(name);
        let port_block = if name == MAIN { None } else { Some(self.block_for(name)) };
        // `main` keeps the configured graft context; a tree's is its own `{tree}/graft`, since the
        // graph it answers about is its own files.
        let graft_dir = if name == MAIN { self.graft_dir.clone() } else { None };
        let ctx = Arc::new(TreeCtx { name: name.to_string(), root: root.clone(), graft: Graft::new(root, graft_dir), port_block });
        map.insert(name.to_string(), ctx.clone());
        ctx
    }

    /// The i-th tree's hundred ports, by the order trees were first served here. A name that has
    /// had a block before keeps it, so a tree re-created after a restart of nothing but this map
    /// does not take a neighbour's.
    fn block_for(&self, name: &str) -> (u16, u16) {
        let mut order = self.order.write().unwrap_or_else(|p| p.into_inner());
        let i = match order.iter().position(|n| n == name) {
            Some(i) => i,
            None => {
                order.push(name.to_string());
                order.len() - 1
            }
        };
        let lo = PORT_BASE + PORT_BLOCK * (i as u16 + 1);
        (lo, lo + PORT_BLOCK - 1)
    }
}


/// Every path OUT of a call, stripped of the tree's root. A result, a listing, a process title, a
/// diff header and an error message all go through here: a model is told one working directory and
/// never the layout around it (spec §3.5).
///
/// The root itself is `.`, not the empty string — a tool that answers a directory answers a path.
/// A path that is somehow not under the root is answered as its own file name rather than leaking
/// the prefix; nothing should produce one, and a leak is the failure that matters.
pub fn relative(tree: &TreeCtx, p: &Path) -> String {
    match p.strip_prefix(&tree.root) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".into(),
        Ok(rest) => rest.to_string_lossy().into_owned(),
        Err(_) => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    }
}

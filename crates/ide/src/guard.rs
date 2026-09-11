//! The preconditions the server refuses to start without. Each is a fact about the pod the
//! spec relies on; a server that started without one would answer wrong quietly.
use crate::Config;

/// The line the workspace image's global git ignore carries (`/etc/kloudlite/gitignore-global`,
/// appended to `~/.config/git/ignore` by the pod prelude). Without it the graph directory this
/// server maintains would show up in every `git status`.
pub const IGNORE_MARKER: &str = "# kloudlite: derived state the platform places inside a workspace directory";

/// The workspace user's uid: `kl`, as the image creates it.
pub const KL_UID: u32 = 1000;

pub fn preflight(cfg: &Config) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    preflight_with(cfg, uid, KL_UID)
}

/// The checks, with the uid injected so a test can run as whoever it runs as.
pub fn preflight_with(cfg: &Config, uid: u32, want_uid: u32) -> Result<(), String> {
    if uid != want_uid {
        return Err(format!("kl ide serve runs as uid {want_uid} (kl), not {uid}"));
    }
    if !cfg.root.is_dir() {
        return Err(format!("the workspace dir {} does not exist; is KL_WORKSPACE set?", cfg.root.display()));
    }
    let ignore = cfg.home.join(".config/git/ignore");
    let has_marker = std::fs::read_to_string(&ignore).map(|s| s.lines().any(|l| l.trim() == IGNORE_MARKER)).unwrap_or(false);
    if !has_marker {
        return Err(format!("{} lacks the platform gitignore block; this image's prelude did not run", ignore.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_names_every_missing_precondition() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config { bind: "127.0.0.1:0".parse().unwrap(), root: tmp.path().join("ws"), home: tmp.path().to_path_buf(), graft_dir: None };
        let why = preflight_with(&cfg, 1000, 1000).unwrap_err();
        assert!(why.contains("workspace dir"), "{why}");
        std::fs::create_dir_all(&cfg.root).unwrap();
        let why = preflight_with(&cfg, 1000, 1000).unwrap_err();
        assert!(why.contains("gitignore"), "{why}");
        std::fs::create_dir_all(tmp.path().join(".config/git")).unwrap();
        std::fs::write(tmp.path().join(".config/git/ignore"), format!("{IGNORE_MARKER}\n.cache/\n")).unwrap();
        assert!(preflight_with(&cfg, 1000, 1000).is_ok());
        assert!(preflight_with(&cfg, 0, 1000).unwrap_err().contains("uid"));
    }
}

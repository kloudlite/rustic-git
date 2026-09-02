use std::path::PathBuf;

pub const DEFAULT_API: &str = "https://dev.kloudlite.io";

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub api: String,
    pub token: String,
    pub expires_at: String,
    pub username: String,
}

/// `KL_CONFIG_DIR` exists so the tests (and anyone juggling two logins) can point the whole CLI at
/// a scratch directory; everything the CLI stores lives under it.
///
/// `~/.config/kl` on EVERY OS, not `dirs::config_dir()` — on macOS that is
/// `~/Library/Application Support/kl`, while the web's copy-paste ssh block and the docs both say
/// `~/.config/kl/known_hosts`. One of the two had to be wrong on a Mac, and a path a person can
/// type is worth more here than the platform convention.
pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("KL_CONFIG_DIR") {
        return d.into();
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        return PathBuf::from(x).join("kl");
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config").join("kl")
}

pub fn path() -> PathBuf {
    dir().join("config.json")
}

pub fn known_hosts() -> PathBuf {
    dir().join("known_hosts")
}

pub fn load() -> Result<Config, String> {
    let p = path();
    let s = std::fs::read_to_string(&p).map_err(|_| "not logged in — run `kl login`".to_string())?;
    serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn save(c: &Config) -> Result<(), String> {
    make_dir(&dir())?;
    // The file holds a 30-day bearer token, so it is never visible at anything but 0600 — the
    // mode goes on the staged file at create time and the rename carries it over.
    write_atomic(&path(), &serde_json::to_string_pretty(c).unwrap())
}

/// Write-then-rename: the old file stays whole until the new one is complete, so a crash or a
/// full disk mid-write cannot leave an empty `~/.ssh/config` behind. `stage` and `commit` are
/// separate only so a test can stand between them.
pub fn write_atomic(p: &std::path::Path, body: &str) -> Result<(), String> {
    commit(&stage(p, body)?, p)
}

fn stage(p: &std::path::Path, body: &str) -> Result<PathBuf, String> {
    use std::io::Write;
    let name = p.file_name().ok_or("no file name")?.to_string_lossy();
    let tmp = p.with_file_name(format!(".{name}.tmp{}", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(body.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    Ok(tmp)
}

fn commit(tmp: &std::path::Path, p: &std::path::Path) -> Result<(), String> {
    std::fs::rename(tmp, p).map_err(|e| e.to_string())
}

/// The config directory holds the token file and known_hosts: nobody else on the machine needs to
/// list it, so it is created 0700 rather than at the umask's discretion.
fn make_dir(d: &std::path::Path) -> Result<(), String> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(d).map_err(|e| e.to_string())
}

/// Pins `<id> <host_key>` in the CLI's own known_hosts. A PIN: once an id has a key, a DIFFERENT
/// key is refused, loudly, the way ssh refuses one — the platform telling us a new key is exactly
/// what a compromised api would also say, and adopting it silently is a man in the middle nobody
/// at either end sees. `KL_ACCEPT_NEW_HOST_KEY=1` is the deliberate escape hatch for a workspace
/// that really was rebuilt.
pub fn pin_host_key(id: &str, host_key: &str) -> Result<(), String> {
    let p = known_hosts();
    make_dir(&dir())?;
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    let stored = old
        .lines()
        .find(|l| l.split_whitespace().next().is_some_and(|h| h == id))
        .map(|l| l[id.len()..].trim().to_string());
    match stored {
        Some(k) if k == host_key => return Ok(()),
        Some(k) if std::env::var("KL_ACCEPT_NEW_HOST_KEY").is_err() => {
            return Err(format!(
                "WARNING: REMOTE HOST KEY HAS CHANGED FOR {id}\n\
                 Someone could be eavesdropping on you right now (man-in-the-middle attack), or the \
                 workspace was rebuilt.\n  stored:   {k}\n  offered:  {host_key}\n\
                 If the workspace really was rebuilt, re-run with KL_ACCEPT_NEW_HOST_KEY=1."
            ));
        }
        _ => {}
    }
    let mut out: String = old
        .lines()
        .filter(|l| !l.split_whitespace().next().is_some_and(|h| h == id))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!("{id} {host_key}\n"));
    write_atomic(&p, &out)
}

#[cfg(test)]
mod tests {
    /// The finding: `pin_host_key` filtered out any existing line for the id and appended whatever
    /// the api just returned, so a changed key was adopted silently on every connect. Chained with
    /// the api's Secret grant, that is a silent MITM of every workspace ssh session — ssh's own
    /// known_hosts would have shouted.
    #[test]
    fn a_changed_host_key_is_refused_not_adopted() {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("KL_CONFIG_DIR", d.path());
        super::pin_host_key("ws-1", "ssh-ed25519 AAAAfirst").unwrap();
        // Same key again is a no-op, not an error: every ssh re-pins.
        super::pin_host_key("ws-1", "ssh-ed25519 AAAAfirst").unwrap();
        let err = super::pin_host_key("ws-1", "ssh-ed25519 AAAAsecond").unwrap_err();
        assert!(err.contains("HOST KEY"), "{err}");
        assert!(err.contains("ws-1"), "{err}");
        // And the stored line is untouched — a refused pin must not half-write the file.
        let kh = std::fs::read_to_string(super::known_hosts()).unwrap();
        assert!(kh.contains("AAAAfirst"), "{kh}");
        assert!(!kh.contains("AAAAsecond"), "{kh}");
        // The override exists so a legitimately rebuilt workspace is not a support ticket.
        std::env::set_var("KL_ACCEPT_NEW_HOST_KEY", "1");
        super::pin_host_key("ws-1", "ssh-ed25519 AAAAsecond").unwrap();
        std::env::remove_var("KL_ACCEPT_NEW_HOST_KEY");
        assert!(std::fs::read_to_string(super::known_hosts()).unwrap().contains("AAAAsecond"));
        // An id with no stored line is a first sight, which is what a pin is FOR.
        super::pin_host_key("ws-2", "ssh-ed25519 AAAAother").unwrap();
    }

    /// The token file must never exist, even briefly, at anything but 0600 — and the directory
    /// that lists it at 0700.
    #[test]
    #[cfg(unix)]
    fn config_is_written_private() {
        use std::os::unix::fs::PermissionsExt;
        the_config_dir_is_dot_config_kl_everywhere();
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("kl");
        std::env::set_var("KL_CONFIG_DIR", &dir);
        let cfg = super::Config {
            api: "https://x".into(),
            token: "t".into(),
            expires_at: "2030".into(),
            username: "k".into(),
        };
        super::save(&cfg).unwrap();
        // A pre-existing world-readable file is replaced, not reopened: the mode is the staged
        // file's, never the old one's.
        std::fs::set_permissions(super::path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        super::save(&cfg).unwrap();

        let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(super::path()), 0o600);
        assert_eq!(mode(dir), 0o700);
    }

    /// A crash between the write and the rename leaves the old file untouched — the staged
    /// bytes sit beside it under another name until the rename makes them the file.
    #[test]
    fn a_crash_before_rename_keeps_the_old_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config");
        std::fs::write(&p, "old").unwrap();
        let tmp = super::stage(&p, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "old");
        assert_eq!(tmp.parent(), p.parent(), "same directory, or rename is a copy");
        assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "new");
        super::commit(&tmp, &p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        assert!(!tmp.exists());
    }

    /// The web's copy-paste block hard-codes `~/.config/kl/known_hosts`, so the CLI has to agree
    /// on every platform — including the Mac, where `dirs::config_dir()` does not.
    ///
    /// Called from `config_is_written_private` rather than run as its own `#[test]`: both touch
    /// process-wide env and cargo runs tests in parallel threads, so as two tests they race over
    /// `KL_CONFIG_DIR`.
    #[cfg(unix)]
    fn the_config_dir_is_dot_config_kl_everywhere() {
        std::env::remove_var("KL_CONFIG_DIR");
        std::env::set_var("XDG_CONFIG_HOME", "/xdg");
        assert_eq!(super::dir(), std::path::Path::new("/xdg/kl"));

        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", "/home/k");
        assert_eq!(super::dir(), std::path::Path::new("/home/k/.config/kl"));
    }
}

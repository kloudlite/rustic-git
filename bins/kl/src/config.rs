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
    let p = path();
    let body = serde_json::to_string_pretty(c).unwrap();
    // The file holds a 30-day bearer token. The mode goes on at CREATE time — chmod after the
    // write leaves a window where the token sits there at whatever the umask allowed — and
    // `set_permissions` still runs because create() does not re-apply the mode to a file that
    // already exists.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&p)
            .map_err(|e| e.to_string())?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&p, body).map_err(|e| e.to_string())?;
    Ok(())
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

/// Pins `<id> <host_key>` in the CLI's own known_hosts, replacing any previous line for that id.
/// The platform tells us the key, so ssh must never be left to ask.
pub fn pin_host_key(id: &str, host_key: &str) -> Result<(), String> {
    let p = known_hosts();
    make_dir(&dir())?;
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    let mut out: String = old
        .lines()
        .filter(|l| !l.split_whitespace().next().is_some_and(|h| h == id))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!("{id} {host_key}\n"));
    std::fs::write(&p, out).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
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
        // A pre-existing world-readable file is the case `create().mode()` alone does not fix,
        // and the reason `set_permissions` is still there.
        std::fs::set_permissions(super::path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        super::save(&cfg).unwrap();

        let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(super::path()), 0o600);
        assert_eq!(mode(dir), 0o700);
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

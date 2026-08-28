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
pub fn dir() -> PathBuf {
    if let Ok(d) = std::env::var("KL_CONFIG_DIR") {
        return d.into();
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("kl")
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
    let d = dir();
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let p = path();
    std::fs::write(&p, serde_json::to_string_pretty(c).unwrap()).map_err(|e| e.to_string())?;
    // The file holds a 30-day bearer token: readable by its owner only, and set after the write
    // rather than before because `write` truncates an existing file, it does not re-create it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Pins `<id> <host_key>` in the CLI's own known_hosts, replacing any previous line for that id.
/// The platform tells us the key, so ssh must never be left to ask.
pub fn pin_host_key(id: &str, host_key: &str) -> Result<(), String> {
    let p = known_hosts();
    std::fs::create_dir_all(dir()).map_err(|e| e.to_string())?;
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    let mut out: String = old
        .lines()
        .filter(|l| !l.split_whitespace().next().is_some_and(|h| h == id))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!("{id} {host_key}\n"));
    std::fs::write(&p, out).map_err(|e| e.to_string())
}

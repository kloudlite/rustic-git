//! Secrets come from files first, the environment second.
//!
//! A value in the process environment shows in `kubectl describe pod`, in every crash dump and in
//! every child process — the worker spawns `git`, the agent spawns `btrfs` and `nix`. A projected
//! file under `KLOUDLITE_SECRETS_DIR` (`/var/run/secrets/kloudlite`, one file per variable name,
//! mode 0440) shows in none of them. The env fallback keeps a local run and an older manifest
//! working; which source answered is logged once per name and the value never is (2026-09-12).

use std::collections::HashSet;
use std::sync::Mutex;

const DEFAULT_DIR: &str = "/var/run/secrets/kloudlite";

static ANNOUNCED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn announce(name: &str, source: &'static str) {
    let mut seen = ANNOUNCED.lock().unwrap_or_else(|e| e.into_inner());
    if seen.get_or_insert_with(HashSet::new).insert(name.to_string()) {
        tracing::info!(name, source, "secret.source");
    }
}

/// The secret named `name`: the projected file's trimmed contents when one exists and is readable,
/// else the environment variable of the same name, else `None`. An empty file or variable is `None`
/// too — an empty secret is a misconfiguration, never a credential.
pub fn read(name: &str) -> Option<String> {
    let dir = std::env::var("KLOUDLITE_SECRETS_DIR").unwrap_or_else(|_| DEFAULT_DIR.to_string());
    if let Ok(s) = std::fs::read_to_string(std::path::Path::new(&dir).join(name)) {
        let s = s.trim();
        if !s.is_empty() {
            announce(name, "file");
            return Some(s.to_string());
        }
    }
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => {
            announce(name, "env");
            Some(v)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::read;

    // One test, not four: every case sets the same two process-global knobs (the directory and an
    // env var), and cargo runs tests in parallel threads.
    #[test]
    fn a_file_wins_over_env_and_both_fall_through_when_empty() {
        let dir = std::env::temp_dir().join(format!("kl-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: this test is the only writer of these two names in this process.
        unsafe { std::env::set_var("KLOUDLITE_SECRETS_DIR", &dir) };
        unsafe { std::env::set_var("KL_TEST_SECRET_A", "from-env") };

        assert_eq!(read("KL_TEST_SECRET_A").as_deref(), Some("from-env"), "no file: env answers");
        std::fs::write(dir.join("KL_TEST_SECRET_A"), "  from-file\n").unwrap();
        assert_eq!(read("KL_TEST_SECRET_A").as_deref(), Some("from-file"), "the file wins, trimmed");
        std::fs::write(dir.join("KL_TEST_SECRET_A"), "\n").unwrap();
        assert_eq!(read("KL_TEST_SECRET_A").as_deref(), Some("from-env"), "an empty file is no secret");
        assert_eq!(read("KL_TEST_SECRET_MISSING"), None, "nowhere: None");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

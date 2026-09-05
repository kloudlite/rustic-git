//! Everything the probe needs to reach the fleet, all from the environment.
//!
//! There is no config file and no flag for any of it: the probe runs as a CronJob whose env is
//! the deployment's own yaml, and a knob a human can pass on the command line is a knob that
//! silently differs between the scheduled run and the one somebody reproduced by hand.

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// `bins/api` with `KLOUDLITE_GIT_API_ROLE=admin` — where every `/admin/*` call goes,
    /// including the run report itself.
    pub admin_url: String,
    /// The ordinary `/v1` process. Deliberately separate from `admin_url`: `sec.user.process`
    /// proves an admin route is absent HERE, which is only meaningful if the two are distinct.
    pub api_url: String,
    pub web_url: String,
    pub git_url: String,
    pub registry: String,
    pub ssh_host: String,
    pub region: String,
    /// Extra hostnames stage 10 resolves and checks a certificate for, beyond the ones above.
    pub hosts: Vec<String>,
    /// The `kloudlite-git-jwt` Secret. The probe mints its own tokens rather than holding a
    /// password, so this is the only credential in the pod.
    pub jwt_secret: String,
    /// The private half of the key `bootstrap` registered for `slo-probe`.
    pub ssh_key_path: String,
    pub kubeconfig: Option<String>,
}

fn req(k: &str) -> Result<String> {
    std::env::var(k).with_context(|| format!("{k} is required"))
}

fn opt(k: &str, default: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Result<Config> {
        Ok(Config {
            admin_url: req("KLOUDLITE_GIT_ADMIN_API_URL")?,
            api_url: req("KLOUDLITE_GIT_API_URL")?,
            web_url: req("KLOUDLITE_GIT_WEB_URL")?,
            git_url: req("KLOUDLITE_GIT_URL")?,
            registry: req("KLOUDLITE_GIT_REGISTRY")?,
            ssh_host: req("KLOUDLITE_GIT_SSH_HOST")?,
            region: req("KLOUDLITE_GIT_REGION")?,
            hosts: opt("KLOUDLITE_GIT_SLO_HOSTS", "")
                .split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect(),
            jwt_secret: req("KLOUDLITE_GIT_JWT_SECRET")?,
            ssh_key_path: opt("KLOUDLITE_GIT_SLO_SSH_KEY", "/etc/slo-ssh/id_ed25519"),
            kubeconfig: std::env::var("KUBECONFIG").ok().filter(|v| !v.trim().is_empty()),
        })
    }
}

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
    /// The ingress address Cloudflare sends `hosts[0]` to. `None` — unset — skips `edge.origin`
    /// rather than inventing an address: a probe that guessed one would report the wrong origin.
    pub origin_ip: Option<String>,
    /// The `kloudlite-git-jwt` Secret. The probe mints its own tokens rather than holding a
    /// password, so this is the only credential in the pod.
    pub jwt_secret: String,
    /// The private half of the key `bootstrap` registered for `slo-probe`.
    pub ssh_key_path: String,
    /// The `known_hosts` line the git tier's SSH listener must present, set from the server's
    /// published key. PINNED, not learned: a probe that ran `ssh-keyscan` and trusted the answer
    /// would report `ssh.hostkey` green through the exact substitution the SLO exists to catch.
    /// Empty means the operator has not pinned one, and `ssh.hostkey` skips rather than passing.
    pub ssh_hostkey: String,
    /// The digest `bootstrap` recorded for `slo-probe/canary`. PINNED for the same reason
    /// `ssh_hostkey` is: a probe that trusted whatever the registry answered would report
    /// `reg.canary` green through the substitution it exists to catch. `None` means unpinned,
    /// and the step skips rather than passing.
    pub canary_digest: Option<String>,
}

fn req(k: &str) -> Result<String> {
    std::env::var(k).with_context(|| format!("{k} is required"))
}

fn opt(k: &str, default: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let ssh_host = req("KLOUDLITE_GIT_SSH_HOST")?;
        check_ssh_host(&ssh_host)?;
        Ok(Config {
            admin_url: req("KLOUDLITE_GIT_ADMIN_API_URL")?,
            api_url: req("KLOUDLITE_GIT_API_URL")?,
            web_url: req("KLOUDLITE_GIT_WEB_URL")?,
            git_url: req("KLOUDLITE_GIT_URL")?,
            registry: req("KLOUDLITE_GIT_REGISTRY")?,
            ssh_host,
            region: req("KLOUDLITE_GIT_REGION")?,
            hosts: opt("KLOUDLITE_GIT_SLO_HOSTS", "")
                .split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect(),
            origin_ip: Some(opt("KLOUDLITE_GIT_SLO_ORIGIN_IP", "")).filter(|v| !v.is_empty()),
            jwt_secret: req("KLOUDLITE_GIT_JWT_SECRET")?,
            ssh_key_path: opt("KLOUDLITE_GIT_SLO_SSH_KEY", "/etc/slo-ssh/id_ed25519"),
            ssh_hostkey: opt("KLOUDLITE_GIT_SLO_SSH_HOSTKEY", ""),
            canary_digest: Some(opt("KLOUDLITE_GIT_SLO_CANARY_DIGEST", "")).filter(|d| !d.is_empty()),
        })
    }
}

impl Config {
    /// `ssh_host` with its port split off. The deployment sets one value because that is what the
    /// web's clone box prints, and everything that dials it needs the two halves apart.
    ///
    /// Accepted shapes, and only these: `host`, `host:port`, `[v6addr]`, `[v6addr]:port`. A bare
    /// IPv6 address is ambiguous — every colon in it looks like a port separator — so it is
    /// REFUSED at boot by `check_ssh_host` rather than silently dialling the wrong port.
    pub fn ssh_endpoint(&self) -> (&str, u16) {
        split_ssh_host(&self.ssh_host).unwrap_or((self.ssh_host.as_str(), 22))
    }
}

fn split_ssh_host(v: &str) -> Option<(&str, u16)> {
    if let Some(rest) = v.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        return match tail {
            "" => Some((addr, 22)),
            t => Some((addr, t.strip_prefix(':')?.parse().ok()?)),
        };
    }
    match v.rsplit_once(':') {
        // Two colons and no brackets is a bare IPv6 address; `check_ssh_host` refused it already.
        Some((h, p)) if !h.contains(':') => Some((h, p.parse().ok()?)),
        Some(_) => None,
        None => Some((v, 22)),
    }
}

/// Refused at boot, not guessed at: `KLOUDLITE_GIT_SSH_HOST` decides which port every SSH step
/// dials, and a value this cannot read would send all three of them at port 22 of the wrong host.
fn check_ssh_host(v: &str) -> Result<()> {
    match split_ssh_host(v) {
        Some(_) => Ok(()),
        None => Err(anyhow::anyhow!(
            "KLOUDLITE_GIT_SSH_HOST must be host, host:port, [v6] or [v6]:port; got `{v}`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ssh_host_is_split_or_refused_never_guessed() {
        assert_eq!(split_ssh_host("git.example.com"), Some(("git.example.com", 22)));
        assert_eq!(split_ssh_host("git.example.com:2222"), Some(("git.example.com", 2222)));
        assert_eq!(split_ssh_host("[::1]:2222"), Some(("::1", 2222)));
        assert_eq!(split_ssh_host("[::1]"), Some(("::1", 22)));
        // A bare v6 address, and a port that is not a number: both refused rather than dialled.
        assert!(check_ssh_host("::1").is_err());
        assert!(check_ssh_host("git.example.com:ssh").is_err());
    }
}

//! The `crane` binary, as the probe uses it.
//!
//! A module rather than inline `tools::run` calls for one reason: the docker config. Every call
//! here sets `DOCKER_CONFIG` to a directory this run owns, so a login never touches `$HOME` (the
//! root filesystem is read-only anyway) and — the part that matters for `reg.visibility` — an
//! ANONYMOUS pull is a `Crane` pointed at an empty directory rather than a flag that could be
//! forgotten. The credential is only ever in that directory and in the argv `tools::run` refuses
//! to format.
//!
//! `--insecure` appears nowhere: the probe reaches the registry the way a person does, over TLS
//! through the same ingress, and a probe that would fall back to plaintext could not tell a
//! working front door from a broken one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

/// A push of a ~1 MiB image over the public ingress; generous because a slow push is a sample,
/// and a cut-off one is only "the probe gave up".
const PUSH_TIMEOUT: Duration = Duration::from_secs(120);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Crane {
    pub bin: String,
    /// `DOCKER_CONFIG`. Two of these exist per run: the logged-in one, and an empty one that is
    /// the whole of what makes `reg.visibility`'s anonymous pull anonymous.
    pub config_dir: PathBuf,
}

impl Crane {
    pub fn new(bin: &str, config_dir: PathBuf) -> Crane {
        Crane { bin: bin.to_string(), config_dir }
    }

    fn env(&self) -> HashMap<String, String> {
        HashMap::from([("DOCKER_CONFIG".into(), self.config_dir.display().to_string())])
    }

    async fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        std::fs::create_dir_all(&self.config_dir)?;
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        crate::tools::run(&self.bin, &args, &self.env(), None, timeout).await
    }

    /// Writes `{config_dir}/config.json`. The password is the personal token stage 1 minted;
    /// `crane auth login` takes it on the argv, which is why `tools::scrub` knows about `-p`.
    pub async fn login(&self, registry: &str, user: &str, password: &str) -> Result<()> {
        self.run(&["auth", "login", registry, "-u", user, "-p", password], READ_TIMEOUT).await?;
        Ok(())
    }

    /// Push an OCI layout DIRECTORY (`crane push` accepts one), so the probe never has to build a
    /// docker tarball or talk to a daemon it does not have.
    pub async fn push(&self, dir: &Path, reference: &str) -> Result<()> {
        self.run(&["push", &dir.display().to_string(), reference], PUSH_TIMEOUT).await?;
        Ok(())
    }

    /// `--format=oci`: the pulled layout is read back off disk by `reg.shared.layer`, and the
    /// default tarball format would hide the per-blob digests that check is entirely about.
    pub async fn pull(&self, reference: &str, dir: &Path) -> Result<()> {
        self.run(&["pull", "--format=oci", reference, &dir.display().to_string()], PUSH_TIMEOUT)
            .await?;
        Ok(())
    }

    pub async fn digest(&self, reference: &str) -> Result<String> {
        Ok(self.run(&["digest", reference], READ_TIMEOUT).await?.trim().to_string())
    }
}

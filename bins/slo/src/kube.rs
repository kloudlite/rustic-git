//! The two things the journey needs a kubeconfig for: running a command inside a workspace pod,
//! and watching a cluster-scoped object converge.
//!
//! Both are reads a person could make with `kubectl`, and both are OPTIONAL: `Ctx::kube` is `None`
//! when no kubeconfig was reachable, and every step that needs one skips rather than failing — a
//! missing kubeconfig is a deployment gap, not an SLO breach.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, AttachParams};
use tokio::io::AsyncReadExt;

/// Run `argv` in `pod` and answer `(exit code, stdout, stderr)`.
///
/// A non-zero exit is NOT an error: `env.detach` passes precisely when a lookup inside the pod
/// fails, so the code has to come back as a value the caller judges rather than as an `Err`.
pub async fn exec(
    c: &kube::Client,
    ns: &str,
    pod: &str,
    container: Option<&str>,
    argv: &[&str],
    timeout: Duration,
) -> Result<(i32, String, String)> {
    let api: Api<Pod> = Api::namespaced(c.clone(), ns);
    let params = AttachParams {
        container: container.map(str::to_string),
        stdout: true,
        stderr: true,
        ..Default::default()
    };
    tokio::time::timeout(timeout, async {
        let mut p = api.exec(pod, argv.iter().copied(), &params).await.context("could not exec")?;
        let mut out = p.stdout().ok_or_else(|| anyhow!("no stdout"))?;
        let mut err = p.stderr().ok_or_else(|| anyhow!("no stderr"))?;
        let status = p.take_status().ok_or_else(|| anyhow!("no status"))?;
        // Both streams drained CONCURRENTLY: the reader ends of the two are one duplex pair, and
        // reading one to the end while the other's buffer fills is a deadlock, not a slow read.
        let (mut o, mut e) = (String::new(), String::new());
        let (a, b) = tokio::join!(out.read_to_string(&mut o), err.read_to_string(&mut e));
        a.context("reading stdout")?;
        b.context("reading stderr")?;
        Ok((code(status.await), o, e))
    })
    .await
    .map_err(|_| anyhow!("exec timed out after {} ms", timeout.as_millis()))?
}

/// The exit code the API server reports for a finished exec. `Success` is 0; a failure carries the
/// real code in a `ExitCode` cause, and anything else we cannot read is 1 — never 0, because a
/// step that reads "the command succeeded" from an answer it did not understand is the one wrong
/// answer here.
fn code(status: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Status>) -> i32 {
    let Some(s) = status else { return 1 };
    if s.status.as_deref() == Some("Success") {
        return 0;
    }
    s.details
        .and_then(|d| d.causes)
        .and_then(|c| {
            c.iter()
                .find(|c| c.reason.as_deref() == Some("ExitCode"))
                .and_then(|c| c.message.as_deref().and_then(|m| m.parse().ok()))
        })
        .unwrap_or(1)
}

/// Poll one cluster-scoped object until `want` holds, or `cap` elapses.
///
/// `want` is handed an `Option`: "the object is gone" is a state the journey waits for
/// (`vol.orphan.collected`), and a helper that could only wait for a present object could not
/// express it.
pub async fn wait_for<K>(
    c: &kube::Client,
    name: &str,
    cap: Duration,
    want: impl Fn(Option<&K>) -> bool,
) -> Result<()>
where
    K: kube::Resource<Scope = kube::core::ClusterResourceScope, DynamicType = ()>
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
{
    let api: Api<K> = Api::all(c.clone());
    let start = std::time::Instant::now();
    let mut why;
    loop {
        match api.get_opt(name).await {
            Ok(v) if want(v.as_ref()) => return Ok(()),
            Ok(_) => why = "it has not converged yet".to_string(),
            Err(e) => why = format!("{e}"),
        }
        if start.elapsed() >= cap {
            return Err(anyhow!("{name} was not there after {} ms: {why}", cap.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Status, StatusCause, StatusDetails};

    /// A status nobody can read must never be reported as a command that worked.
    #[test]
    fn only_an_explicit_success_is_exit_zero() {
        assert_eq!(code(Some(Status { status: Some("Success".into()), ..Default::default() })), 0);
        assert_eq!(code(None), 1);
        let failed = Status {
            status: Some("Failure".into()),
            details: Some(StatusDetails {
                causes: Some(vec![StatusCause {
                    reason: Some("ExitCode".into()),
                    message: Some("7".into()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(code(Some(failed)), 7);
    }
}

//! The three refusals: a workspace whose space uses no environment, a port the service does not
//! declare, a mapping onto the tool server's port — and the UDP one nothing can provoke yet.
//!
//! None of them needs an intercept in force, which is why they run FIRST: the "no environment"
//! case clears the space for a moment, and that would release an intercept that already was.

use super::*;

/// `env.intercept.refused`: the two guards that stop an intercept pointing traffic somewhere
/// nobody authorised. The status AND the sentence, because a 409 that names nothing leaves a
/// person guessing. The first case clears the space and puts the choice back before the second.
pub(super) async fn refused(c: &mut Ctx, j: &Journey) {
    let (e, w, space) = (j.env.clone(), j.ws.clone(), j.space(c));
    c.step("env.intercept.refused", REFUSED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts"));
        let unattached = serde_json::json!({ "service": TARGET, "workspace": w, "ports": [] });
        // A port the service does not declare, on a workspace whose space DOES use it — so the
        // only thing wrong with the request is the port.
        let bad_port = intercept_body(&w, 9999, WS_PORT);
        async move {
            call(c, reqwest::Method::DELETE, &space, &jwt, None).await.context("could not clear the space")?;
            let first =
                expect_refusal(c, &url, &jwt, unattached, reqwest::StatusCode::CONFLICT, "does not use this environment").await;
            call(c, reqwest::Method::PUT, &space, &jwt, Some(serde_json::json!({ "environment": e })))
                .await
                .context("could not choose the environment again")?;
            first?;
            expect_refusal(c, &url, &jwt, bad_port, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "9999").await
        }
        .boxed()
    })
    .await;
}

/// `env.intercept.tools.refused`: a mapping onto the tool server's port is refused naming it.
///
/// The tool server listens on the pod IP with no auth of its own, so an intercept that mapped a
/// service's port onto 7788 would hand every pod in the environment an `exec` in the workspace.
pub(super) async fn tools_refused(c: &mut Ctx, j: &Journey) {
    let (e, w) = (j.env.clone(), j.ws.clone());
    c.step("env.intercept.tools.refused", REFUSED_CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let url = api(c, &format!("/v1/environments/{e}/intercepts"));
        let body = intercept_body(&w, TARGET_PORT, k8s::IDE_PORT);
        async move { expect_refusal(c, &url, &jwt, body, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "7788").await }.boxed()
    })
    .await;
}

/// `env.intercept.udp.refused`: skipped, with the reason, until a service can declare a UDP port.
///
/// `model::Service` carries no protocol and `service_clusterip` hard-codes TCP, so nothing in an
/// environment can be UDP and the probe has no way to provoke the refusal. A skip is visible in the
/// report; a silent pass would report a guard nobody has as kept.
pub(super) fn udp_refused(c: &mut Ctx) {
    c.skip("env.intercept.udp.refused", "model::Service carries no protocol yet");
}

pub(super) async fn expect_refusal(
    c: &Ctx,
    url: &str,
    jwt: &str,
    body: Value,
    want: reqwest::StatusCode,
    names: &str,
) -> Result<()> {
    let (status, text) = raw(c, reqwest::Method::POST, url, jwt, Some(body), &[]).await?;
    if status != want {
        return Err(anyhow!("the intercept answered {status}, not {want}: {}", text.trim()));
    }
    if !text.contains(names) {
        return Err(anyhow!("the refusal does not say what was wrong ({names:?}): {}", text.trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool server's port is the one the refusal is about, and it is read from the platform
    /// rather than typed twice: the catalogue row and `deploy/slo.md` both name 7788.
    #[test]
    fn the_tools_refusal_names_the_tool_server_port() {
        assert_eq!(k8s::IDE_PORT, 7788);
        assert_eq!(intercept_body("ws-1", TARGET_PORT, k8s::IDE_PORT)["ports"][0]["workspace"], 7788);
    }
}

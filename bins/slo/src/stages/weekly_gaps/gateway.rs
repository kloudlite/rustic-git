//! Weekly drill on the gateway's capabilities through a real tunnel.

use super::*;


/// `gw.caps`: the two things between one person and the region's gateway.
///
/// A connect token is spent on use (`Gateway::spend`) and a workspace may hold ten tunnels; both
/// are refusals, and a gateway that stopped enforcing either would look perfectly healthy to every
/// other id. The replay half is first because it costs one request; the cap half opens ten real
/// tunnels and requires the eleventh to be refused, then closes all of them.
pub(crate) async fn gw_caps(c: &mut Ctx, cold: Option<&str>) {
    let Some(ws) = cold.map(str::to_string) else {
        return c.skip("gw.caps", "no cold workspace");
    };
    let key = c.cfg.ssh_key_path.clone();
    c.step("gw.caps", TUNNEL_CEILING, move |c| {
        async move {
            // Started first: stage 7 parks the workspace's pod as soon as its own ids are done
            // (the region's nodes cannot hold four at once), and a tunnel needs something running.
            let doc = api(c, &format!("/v1/workspaces/{ws}"));
            post(c, &api(c, &format!("/v1/workspaces/{ws}/start")), &c.probe_jwt.clone(), Value::Null)
                .await
                .context("could not start the workspace to tunnel into")?;
            poll_json(c, &doc, &c.probe_jwt.clone(), Duration::from_secs(120), |v| {
                v.get("state").and_then(Value::as_str) == Some("ready")
            })
            .await
            .context("the workspace never came back ready for the tunnel")?;
            // One session, spent twice. The second connect must be refused: a CONNECT token is
            // one-shot, and replaying one is either a bug or an attack.
            let session = super::super::workspace::ssh_session(c, &ws).await?;
            let (ssh, kl) = (c.programs.ssh.clone(), c.programs.kl.clone());
            let args = super::super::workspace::ssh_args(&kl, &key, &ws);
            let env = super::super::workspace::session_env(&session);
            tools::run(&ssh, &args, &env, None, Duration::from_secs(30))
                .await
                .context("the first use of a connect token failed")?;
            if tools::run(&ssh, &args, &env, None, Duration::from_secs(30)).await.is_ok() {
                return Err(anyhow!("a connect token was accepted twice"));
            }

            // The per-workspace cap. Each tunnel is its own ssh, held open by a sleep; the
            // eleventh is judged by the GATEWAY's own answer — ssh exits when the tunnel is
            // refused — waited for with a bound rather than sampled a second after spawn, which
            // read a still-handshaking child as "the cap is not enforced".
            let mut open = vec![];
            for i in 0..MAX_PER_WS {
                let session = super::super::workspace::ssh_session(c, &ws).await?;
                match spawn_tunnel(&ssh, &kl, &key, &ws, &session) {
                    Ok(ch) => open.push(ch),
                    Err(e) => {
                        for mut ch in open {
                            let _ = ch.kill().await;
                        }
                        return Err(anyhow!("tunnel {i} could not be opened at all: {e}"));
                    }
                }
            }
            // A moment for the ten to be counted by the gateway before the eleventh asks.
            tokio::time::sleep(Duration::from_secs(5)).await;
            let session = super::super::workspace::ssh_session(c, &ws).await?;
            let over = match spawn_tunnel(&ssh, &kl, &key, &ws, &session) {
                Ok(mut ch) => {
                    let out = tokio::time::timeout(OVER_CAP, ch.wait()).await;
                    let _ = ch.kill().await;
                    match out {
                        // Refused: ssh exited on its own, non-zero, inside the window.
                        Ok(Ok(status)) => !status.success(),
                        Ok(Err(_)) => false,
                        // Still pumping after the window: the eleventh tunnel is live.
                        Err(_) => false,
                    }
                }
                Err(_) => true,
            };
            for mut ch in open {
                let _ = ch.kill().await;
            }
            if !over {
                return Err(anyhow!("an eleventh tunnel to one workspace stayed open: the cap of {MAX_PER_WS} is not being enforced"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// One held-open tunnel, as a child process. `kill_on_drop` so a step that times out takes its
/// tunnels with it rather than leaving slots held until the gateway's 30-minute idle close.
pub(crate) fn spawn_tunnel(
    ssh: &str,
    kl: &str,
    key: &str,
    ws: &str,
    session: &str,
) -> std::io::Result<tokio::process::Child> {
    // `ssh_args` ends in the command `true`; pushing after it would run `true sleep 60`, which
    // exits at once — ten tunnels the gateway counted for 300 ms each, and an eleventh that
    // "stayed open" against a count of zero. The held tunnel replaces the command.
    let mut argv = super::super::workspace::ssh_args(kl, key, ws);
    argv.pop();
    argv.push("sleep 60".into());
    tokio::process::Command::new(ssh)
        .args(&argv)
        .envs(super::super::workspace::session_env(session))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

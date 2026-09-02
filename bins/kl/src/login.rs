//! The device-code login. Nothing the CLI holds before approval is a credential, so the code can
//! be printed, read aloud, and pasted into whatever browser the person is already signed in to.

use crate::config::{self, Config};

#[derive(serde::Deserialize)]
struct DeviceCode {
    code: String,
    poll: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliToken {
    token: String,
    expires_at: String,
}

pub async fn login(api: String) -> Result<(), String> {
    let api = api.trim_end_matches('/').to_string();
    let device = hostname();
    let c = crate::api::client();
    let dc: DeviceCode = c
        .post(format!("{api}/v1/cli/code"))
        .json(&serde_json::json!({ "device": device }))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("asking {api} for a login code: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let url = format!("{api}/cli/authorize?code={}", dc.code);
    println!("Confirm this code in your browser: {}", dc.code);
    println!("{url}");
    // A machine with no browser (a server over ssh) is the normal case, not an error — the URL is
    // printed above either way.
    let _ = open::that_detached(&url);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    // Complained about once, not on every retry: an api hiccup that clears itself should not
    // scroll the approval URL off the screen.
    let mut complained = false;
    loop {
        if std::time::Instant::now() > deadline {
            return Err("timed out waiting for approval".into());
        }
        let r = match c
            .get(format!("{api}/v1/cli/token"))
            .query(&[("poll", &dc.poll)])
            .send()
            .await
        {
            Ok(r) => r,
            // A dropped connection mid-login is worth retrying: the code is still valid until the
            // api expires it, and that is what the 410 below reports.
            Err(e) => {
                if !complained {
                    eprintln!("kl: still trying ({e})");
                    complained = true;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        match r.status().as_u16() {
            202 => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            200 => {
                let t: CliToken = r.json().await.map_err(|e| e.to_string())?;
                let username = username_of(&t.token).unwrap_or_default();
                config::save(&Config {
                    api,
                    token: t.token,
                    expires_at: t.expires_at,
                    username: username.clone(),
                })?;
                println!(
                    "Logged in as {username}. Config: {}",
                    config::path().display()
                );
                return Ok(());
            }
            // 410 is the api's one terminal answer: expired, denied, or already spent.
            410 => return Err("that login expired or was denied — run `kl login` again".into()),
            other => {
                if !complained {
                    eprintln!("kl: still trying (the api answered {other})");
                    complained = true;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

pub async fn logout() -> Result<(), String> {
    let cfg = config::load()?;
    // Best effort: a token we cannot revoke server-side still must not stay on this disk.
    if let Some(jti) = claim(&cfg.token, "jti") {
        let _ = crate::api::client()
            .delete(format!("{}/v1/cli/tokens/{jti}", cfg.api))
            .bearer_auth(&cfg.token)
            .send()
            .await;
    }
    std::fs::remove_file(config::path()).map_err(|e| e.to_string())?;
    println!("Logged out.");
    Ok(())
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn username_of(token: &str) -> Option<String> {
    claim(token, "username")
        .or_else(|| claim(token, "name"))
        .or_else(|| claim(token, "sub"))
}

/// Reads one claim out of the JWT payload. No verification: the signature is the api's business,
/// and the CLI only ever uses this for its own display name and the revocation id.
fn claim(token: &str, key: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get(key)?.as_str().map(str::to_string)
}

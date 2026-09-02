use crate::api;
use crate::config;

pub async fn list(team: Option<&str>) -> Result<(), String> {
    let cfg = config::load()?;
    let ws = api::list(&cfg, team).await.map_err(|e| e.to_string())?;
    println!("{:<20} {:<24} {:<10} PACKAGES", "NAME", "ID", "STATE");
    for w in &ws {
        println!("{:<20} {:<24} {:<10} {}", w.name, w.id, w.state, w.packages.join(","));
    }
    Ok(())
}

pub async fn ssh(target: &str, args: &[String]) -> Result<(), String> {
    let cfg = config::load()?;
    // ONE api call before the handshake: the api resolves a name to an id, and the session it
    // mints rides to the ProxyCommand child in ssh's environment rather than being minted again.
    // The host key is pinned here because ssh reads known_hosts in the parent process, before the
    // proxy has run even once; a change from what's stored is refused, not adopted.
    let s = api::ssh_session(&cfg, target).await.map_err(|e| e.to_string())?;
    let id = &s.id;
    // The id lands in ssh's argv and in a /bin/sh-parsed ProxyCommand; the api is trusted for
    // its content but not for its shape.
    if !crate::sshconfig::safe_name(id) {
        return Err(format!("workspace id {id:?} cannot be passed to ssh"));
    }
    config::pin_host_key(id, &s.host_key)?;

    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new("ssh");
    cmd.arg("-o")
        // Quoted: ssh runs the ProxyCommand through /bin/sh, and the binary's own path can hold
        // spaces (`~/Library/Application Support/…`, `C:\Program Files\…`). Double quotes are
        // what ssh's tokeniser accepts here.
        .arg(format!("ProxyCommand=\"{}\" ws proxy {id}", me.display()))
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", config::known_hosts().display()))
        .arg("-o")
        .arg(format!("HostKeyAlias={id}"))
        .arg(format!("kl@{id}"))
        .args(args)
        // The token in a child's environment is readable by this user's other processes — the
        // same user who holds the CLI token it was minted from, and it expires in minutes.
        .env(crate::proxy::SESSION_ENV, serde_json::to_string(&s).map_err(|e| e.to_string())?);
    // exec, not spawn: ssh owns the terminal (job control, window resizes, the exit status) and a
    // parent sitting in the middle only gets those wrong.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(format!("running ssh: {}", cmd.exec()))
    }
    #[cfg(not(unix))]
    {
        let st = cmd.status().map_err(|e| e.to_string())?;
        std::process::exit(st.code().unwrap_or(1));
    }
}

pub async fn ssh_config() -> Result<(), String> {
    let cfg = config::load()?;
    let ws = api::list(&cfg, None).await.map_err(|e| e.to_string())?;
    let p = crate::sshconfig::write(&ws)?;
    println!("Wrote {} ({} workspaces).", p.display(), ws.len());
    // The generated blocks say `ProxyCommand kl …`, which ssh resolves through PATH — from a
    // desktop launcher's environment, not this shell's. Better a note now than "Connection closed
    // by remote host" later.
    if !on_path("kl") {
        println!("Note: `kl` is not on your PATH; ssh will not find the ProxyCommand.");
        println!("      Add {} to PATH.", std::env::current_exe().map(|p| p.parent().map(|d| d.display().to_string()).unwrap_or_default()).unwrap_or_default());
    }
    Ok(())
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

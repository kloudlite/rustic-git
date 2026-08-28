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

/// Names are what people type; ids are what everything else uses. An exact id wins over a name so
/// a workspace named after another's id cannot shadow it.
async fn resolve(cfg: &config::Config, target: &str) -> Result<String, String> {
    let ws = api::list(cfg, None).await.map_err(|e| e.to_string())?;
    if let Some(w) = ws.iter().find(|w| w.id == target) {
        return Ok(w.id.clone());
    }
    match ws.iter().find(|w| w.name == target) {
        Some(w) => Ok(w.id.clone()),
        None => Err(format!("no workspace named {target}")),
    }
}

pub async fn ssh(target: &str, args: &[String]) -> Result<(), String> {
    let cfg = config::load()?;
    let id = resolve(&cfg, target).await?;
    // Pin the host key before ssh starts: the ProxyCommand pins it too, but ssh reads known_hosts
    // in the parent process, before the proxy has run even once.
    let s = api::ssh_session(&cfg, &id).await.map_err(|e| e.to_string())?;
    config::pin_host_key(&id, &s.host_key)?;

    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new("ssh");
    cmd.arg("-o")
        .arg(format!("ProxyCommand={} ws proxy {id}", me.display()))
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", config::known_hosts().display()))
        .arg("-o")
        .arg(format!("HostKeyAlias={id}"))
        .arg(format!("root@{id}"))
        .args(args);
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
    Ok(())
}

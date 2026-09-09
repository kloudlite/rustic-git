//! Attaching a workspace to an environment: the per-workspace `resolv.conf` the agent renders
//! (`attach_file`) and mounts read-only, and the projected `authorized_keys` file — both hostPath
//! files written in place, never renamed, because the pod holds the inode.

use super::*;


/// The agent-owned directory holding one rendered `resolv.conf` per workspace. Outside any user
/// volume on purpose: it is platform state, so it is never in a snapshot and never pushed.
pub fn attach_root(pool: &str) -> String {
    format!("{pool}/attach")
}


pub fn attach_dir(pool: &str, ws_id: &str) -> String {
    format!("{}/{ws_id}", attach_root(pool))
}


pub fn attach_file(pool: &str, ws_id: &str) -> String {
    format!("{}/resolv.conf", attach_dir(pool, ws_id))
}


/// This workspace's rendered `resolv.conf`. A FILE, and mounted as one: the agent rewrites it in
/// place precisely because the pod holds the inode.
pub(super) fn attach_volume(pool: &str, ws_id: &str) -> Volume {
    Volume {
        name: "attach".to_string(),
        host_path: Some(HostPathVolumeSource { path: attach_file(pool, ws_id), type_: Some("File".into()) }),
        ..Default::default()
    }
}


/// Where the agent renders each owner namespace's `authorized_keys` from its `OwnerKeys`
/// projection. Platform state like `attach_root`: outside any user volume, so it is never
/// snapshotted and never pushed.
pub fn keys_root(pool: &str) -> String {
    format!("{pool}/keys")
}


pub fn keys_file(pool: &str, owner: &str) -> String {
    format!("{}/{owner}/authorized_keys", keys_root(pool))
}


/// The owner's `authorized_keys`, a FILE the agent rewrites in place for `attach_volume`'s
/// reason: the pod holds the inode, so a rename would leave sshd reading the old file forever.
/// `type: File` is also what parks the pod until the agent has written it — a missing file is
/// "keys not ready", never "no keys" (that is an empty file).
pub(super) fn keys_volume(pool: &str, owner: &str) -> Volume {
    Volume {
        name: "authorized-keys".to_string(),
        host_path: Some(HostPathVolumeSource { path: keys_file(pool, owner), type_: Some("File".into()) }),
        ..Default::default()
    }
}


/// The `/etc/resolv.conf` a workspace pod gets, rendered from the AGENT's own file.
///
/// Templated rather than synthesised: the agent is not `hostNetwork`, so kubelet wrote its file
/// with the cluster nameserver, `options ndots:5` and the node's DNS suffix already in it. Copying
/// those means they can never drift from what the cluster actually uses; only the search line —
/// the one thing that is per-pod — is replaced.
///
/// The environment's namespace goes first so a service it defines wins over a same-named service
/// in the workspace's own namespace.
pub fn resolv_conf(template: &str, ws_ns: &str, env_ns: Option<&str>) -> String {
    // The cluster domain is itself one of the values this function exists to avoid hardcoding —
    // a cluster started with `--cluster-domain=cluster.internal` must not get `cluster.local`
    // search entries. Recover it from the template's own search line (kubelet always writes
    // `search <ns>.svc.<domain> svc.<domain> <domain> ...`) rather than assuming the default.
    let domain = template
        .lines()
        .find(|l| l.starts_with("search "))
        .and_then(|l| l.split_whitespace().find_map(|tok| tok.strip_prefix("svc.")))
        .unwrap_or("cluster.local");

    let mut search = String::from("search ");
    if let Some(env) = env_ns {
        search.push_str(&format!("{env}.svc.{domain} "));
    }
    search.push_str(&format!("{ws_ns}.svc.{domain} svc.{domain} {domain}"));
    // Whatever the node appends after the cluster domains (a cloud's internal zone) is carried
    // over verbatim: it is how a pod resolves node-local names and we have no business guessing it.
    if let Some(tail) = template
        .lines()
        .find(|l| l.starts_with("search "))
        .and_then(|l| l.split_once(&format!(" {domain}")))
        .map(|(_, rest)| rest.trim_end())
        .filter(|rest| !rest.is_empty())
    {
        search.push_str(tail);
    }
    let rest: Vec<&str> = template.lines().filter(|l| !l.starts_with("search ")).collect();
    let mut out = search;
    for line in rest {
        out.push('\n');
        out.push_str(line);
    }
    out.push('\n');
    out
}

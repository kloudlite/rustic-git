//! Domain models, mirrored 1:1 against the Cosmos JSON in
//! docs/superpowers/specs/2026-08-24-workspaces-environments-design.md §Domain model.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;


#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub storage_account: String,
    pub blob_container: String,
    pub status: String,
}



#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WsState {
    Creating,
    Ready,
    Stopped,
    Error,
    Deleted,
}

/// Every materialized workspace runs a container (`ws-{id}`) with its live subvolume
/// bind-mounted. `alpine` by default, and nothing more: the tools come from the Nix profile
/// (`spec.packages` on top of the platform's base set), so the image only has to exist, be
/// small, and stay alive — `k8s::workspace_pod` gives it `sleep infinity` for that.
pub fn default_ws_image() -> String {
    DEFAULT_WS_IMAGE.into()
}

/// The MARKER a spec carries for "the platform's image" — untagged on purpose. The tag is the
/// agent's business (`WS_DEFAULT_IMAGE`, pinned with the agent by pin.sh): a spec that froze a
/// tag would pin every workspace to whatever the image was the day it was created.
pub const DEFAULT_WS_IMAGE: &str = "ghcr.io/kloudlite/rustic-git-workspace";

/// Whether a spec's image means "the platform's own": the marker, a tagged form of it, or the
/// two images the platform used to default to — specs written back then must keep getting sshd.
pub fn is_default_image(image: &str) -> bool {
    image == DEFAULT_WS_IMAGE
        || image.strip_prefix(DEFAULT_WS_IMAGE).is_some_and(|rest| rest.starts_with(':') || rest.starts_with('@'))
        || image == "alpine:3.20"
}

/// What a client needs to ssh in, minus the ticket: the URL to dial and the key to pin.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SshDoc {
    pub gateway: String,
    pub host_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub owner: String,
    /// Empty for personal. See `crd::WorkspaceSpec::team`.
    #[serde(default)]
    pub team: String,
    pub name: String,
    pub region: String,
    pub state: WsState,
    #[serde(default = "default_ws_image")]
    pub image: String,
    pub placement: Option<String>,
    /// Pointer to the workspace's storage registry volume (`vol/{owner}/{id}`), written by the
    /// job-done handler once the workspace has first pushed; `None` until then. `ref` is kept as
    /// an alias so docs written before the commit/push split still deserialize.
    #[serde(alias = "ref")]
    pub volume: Option<String>,
    pub quota_gb: u64,
    /// `None` until the workspace's pod has reported a host key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshDoc>,
    /// Current live state: exposed ports, installed packages, free-form. Snapshotted into the
    /// `Snapshot` record's `state` at push time. Named `live_state` (not `state`, despite the
    /// design doc's JSON sketch reusing that key) because the field above already owns `state`
    /// for the lifecycle enum.
    #[serde(default)]
    pub live_state: serde_json::Value,
    /// The list the caller asked for (`spec.packages`), not what is installed — see
    /// `packages_status` for what building it produced.
    #[serde(default)]
    pub packages: Vec<String>,
    /// The platform's base set the node built the profile with — shown, never edited here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_packages: Vec<String>,
    /// `None` until the reconciler has said anything about the list, which the web renders as
    /// "installing…" rather than as a failure that was never reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages_status: Option<PackagesDoc>,
}

/// The `PackagesReady` condition, flattened for the web.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackagesDoc {
    pub ready: bool,
    pub reason: String,
    pub message: String,
}

/// Names a folder inside the env's own subvolume (`live/volumes/{folder}`), never a workspace —
/// see the "An environment is a composition" decision in the design doc. Any non-empty `folder`
/// name must be a single safe segment (see `validate_mount` — anything else escapes the
/// subvolume); the folder is created on demand when the environment is materialized
/// (`mkdir_env_mounts` in the controller). `#[serde(alias)]` keeps old docs
/// (and the API request body) that still say `volume` working.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Mount {
    #[serde(alias = "volume")]
    pub folder: String,
    pub path: String,
}

/// A mount is bind-mounted by a ROOT agent, so both halves are a security boundary, not a
/// convenience check: `folder` is joined onto the environment's own subvolume and `path` is
/// concatenated into a `src:dst` string. `Path::join` discards the base when the component is
/// absolute and `..` walks out of it, so anything but a single safe segment hands the caller an
/// arbitrary host path; a `:` in either half splices extra fields (a second mapping, `:ro`) into
/// the bind string. Kept here rather than in `engine::compose` so the runtime that replaces
/// compose can call the same rule.
pub fn validate_mount(m: &Mount) -> Result<(), String> {
    if !rustic_git_storage::store::valid_segment(&m.folder) {
        return Err(format!("mount folder {:?} must be a single name of [A-Za-z0-9._-]", m.folder));
    }
    if !m.path.starts_with('/') || m.path.contains(':') || m.path.contains('\0') {
        return Err(format!("mount path {:?} must be an absolute path with no ':'", m.path));
    }
    Ok(())
}

/// A service's name becomes a StatefulSet, a ClusterIP Service and a label value, so it has to be
/// a DNS-1035 label (`[a-z]([-a-z0-9]*[a-z0-9])?`, at most 63): anything else is a 422 from the
/// API server on EVERY reconcile, forever, and the environment never comes up. Ports and env keys
/// are checked here for the same reason — the API server, not this code, is what rejects a port 0
/// or a `FOO-BAR` env name, and it does so one requeue at a time.
pub fn validate_service(s: &Service) -> Result<(), String> {
    let n = s.name.as_bytes();
    let label = !n.is_empty()
        && n.len() <= 63
        && n[0].is_ascii_lowercase()
        && n[n.len() - 1] != b'-'
        && n.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if !label {
        return Err(format!("service name {:?} must be a lowercase DNS label starting with a letter", s.name));
    }
    if s.ports.contains(&0) {
        return Err(format!("service {:?}: port must be 1-65535", s.name));
    }
    for k in s.env.keys() {
        let ok = k.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !ok {
            return Err(format!("service {:?}: env name {k:?} must match [A-Za-z_][A-Za-z0-9_]*", s.name));
        }
    }
    s.mounts.iter().try_for_each(validate_mount)
}

/// Every service, plus the rule no single service can check: two with one name are one
/// StatefulSet, and the second silently overwrites the first.
pub fn validate_services(services: &[Service]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for s in services {
        validate_service(s)?;
        if !seen.insert(s.name.as_str()) {
            return Err(format!("duplicate service name {:?}", s.name));
        }
    }
    Ok(())
}

/// A workspace name is written VERBATIM into generated ssh config — `Host {name}` in
/// `bins/kl/src/sshconfig.rs` and in the web's copy block. A newline in it appends arbitrary
/// keywords (`ProxyCommand`, `Host *`) to a teammate's `~/.ssh` on the next `kl ws ssh-config`,
/// so this is a security boundary and not a tidiness rule. Same alphabet as `valid_segment`,
/// capped at 63 so a name can never be the reason a DNS label has to be truncated.
pub fn valid_ws_name(name: &str) -> bool {
    // The name is also the directory the workspace mounts at inside the person's home
    // (`~/workspaces/<name>`), so `.` and `..` — otherwise legal by the character rule — would
    // mount a workspace over the home itself.
    !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && name.bytes().any(|b| b != b'.')
}

/// Every untrusted string on a `WorkspaceSpec` that becomes a path, an argv word or an object
/// name, checked in ONE place at the agent.
///
/// `/v1` checks these on write, but `/v1` is not the only writer: a restored backup, a migration
/// or an operator with kubectl produces a spec no handler ever saw, and the agent's own builders
/// splice these into a root `/bin/sh -c` prelude and into `{pool}/homes/{owner}`. Same rule, same
/// reason, as `git_init_container`'s repo/branch re-check.
pub fn validate_ws_spec(spec: &crate::crd::WorkspaceSpec) -> Result<(), String> {
    if !valid_ws_name(&spec.name) {
        return Err(format!("workspace name {:?} is not a name", spec.name));
    }
    validate_owner(&spec.owner)?;
    if !spec.team.is_empty() && !rustic_git_storage::store::valid_segment(&spec.team) {
        return Err(format!("team {:?} is not a segment", spec.team));
    }
    // Packages are deliberately NOT checked here: the profile build already validates them and
    // reports the far more useful `PackagesReady` condition, which this would pre-empt.
    Ok(())
}

/// `spec.owner` alone — the half an `Environment` shares. It is joined onto the pool root and
/// chowned by a privileged process (`ensure_shared_home`, `ensure_homecache`), so a traversal here
/// is a root-run `mkdir`/`chown` outside the pool.
pub fn validate_owner(owner: &str) -> Result<(), String> {
    match rustic_git_storage::store::valid_owner(owner) {
        true => Ok(()),
        false => Err(format!("owner {owner:?} is not an owner name")),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Service {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub mounts: Vec<Mount>,
    /// Container ports this service answers on, published as a ClusterIP Service so siblings — and
    /// an attached workspace — can reach it by service name. `default` so environment documents
    /// written before ports existed still deserialize as "exposes nothing".
    #[serde(default)]
    pub ports: Vec<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvState {
    Creating,
    Running,
    Stopped,
    Error,
    Deleted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Environment {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub region: String,
    pub state: EnvState,
    pub placement: Option<String>,
    /// The env's OWN storage registry volume pointer (`vol/{owner}/{id}`; one btrfs subvolume
    /// for the whole env, every mount a folder inside it) — moved by etag CAS exactly like
    /// `Workspace.volume`.
    #[serde(alias = "ref", default)]
    pub volume: Option<String>,
    pub services: Vec<Service>,
    /// The snapshot the environment's disk last landed on, when a restore put one there. Read off
    /// the child `Volume`'s status, and absent for every environment that has never been restored
    /// in place — where "current" is simply the newest record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_to: Option<String>,
    /// When that restore was asked for. With `restored_to` it is what tells a snapshot taken AFTER
    /// the restore (the environment moved on to it) from one the restored record already had as a
    /// child before (a sibling branch the environment is not on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_requested_at: Option<String>,
    /// Why this environment is mid-restore, as the condition's own `reason` (`Draining` while its
    /// services stop, `Restoring` while the disk is swapped). `None` is the ordinary state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restoring: Option<String>,
}




#[cfg(test)]
mod tests {
    use super::{validate_mount, validate_services, Mount, Service};

    fn m(folder: &str, path: &str) -> Mount {
        Mount { folder: folder.into(), path: path.into() }
    }

    fn svc(name: &str) -> Service {
        Service { name: name.into(), image: "alpine".into(), command: vec![], env: Default::default(), mounts: vec![], ports: vec![80] }
    }

    #[test]
    fn a_service_is_refused_before_the_api_server_would_refuse_it_forever() {
        assert!(validate_services(&[svc("db"), svc("web-1")]).is_ok());
        for bad in ["Foo_bar", "", "-db", "db-", "1db", &"a".repeat(64)] {
            assert!(validate_services(&[svc(bad)]).is_err(), "{bad:?}");
        }
        assert!(validate_services(&[svc("db"), svc("db")]).is_err(), "duplicates overwrite a sibling");
        let mut p0 = svc("db");
        p0.ports.push(0);
        assert!(validate_services(&[p0]).is_err(), "port 0");
        let mut env = svc("db");
        env.env.insert("FOO-BAR".into(), "x".into());
        assert!(validate_services(&[env]).is_err(), "env key");
        let mut ok_env = svc("db");
        ok_env.env.insert("_FOO1".into(), "x".into());
        assert!(validate_services(&[ok_env]).is_ok());
        let mut esc = svc("db");
        esc.mounts.push(m("../etc", "/etc"));
        assert!(validate_services(&[esc]).is_err(), "mounts are still checked here");
    }

    #[test]
    fn a_mount_folder_is_one_safe_segment() {
        assert!(validate_mount(&m("data", "/data")).is_ok());
        assert!(validate_mount(&m("pg_data-1.2", "/var/lib/postgresql")).is_ok());

        // Every one of these bind-mounts something outside the environment's own subvolume,
        // because `Path::join` drops the base on an absolute component and `..` walks out.
        for bad in ["/", "..", "a/b", "", ".", "../../etc", "/etc", "a:b", "x\0y"] {
            assert!(validate_mount(&m(bad, "/data")).is_err(), "folder {bad:?} must be refused");
        }
    }

    #[test]
    fn a_workspace_name_cannot_carry_ssh_config() {
        for ok in ["dev", "my-ws.2", "a_b", &"x".repeat(63)] {
            assert!(super::valid_ws_name(ok), "name {ok:?} must be allowed");
        }
        // The newline cases are the injection; the rest are the alphabet and the length.
        for bad in [
            "",
            "x\n  ProxyCommand /bin/sh -c curl|sh\nHost *",
            "a b",
            "a\tb",
            "a/b",
            "a*",
            "a\r",
            &"x".repeat(64), ".", "..", "...",
        ] {
            assert!(!super::valid_ws_name(bad), "name {bad:?} must be refused");
        }
    }

    #[test]
    fn a_mount_path_is_absolute_and_colon_free() {
        assert!(validate_mount(&m("data", "/data")).is_ok());
        for bad in ["", "data", "./data", "/data:ro", "/data:/etc:ro", "/data\0"] {
            assert!(validate_mount(&m("data", bad)).is_err(), "path {bad:?} must be refused");
        }
    }
}

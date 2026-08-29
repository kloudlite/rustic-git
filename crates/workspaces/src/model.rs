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
    /// Per-region shared secret agents present on every `/v1/agent/*` request. `default` so
    /// older region docs (written before this field existed) still deserialize.
    #[serde(default)]
    pub agent_token: String,
}



#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WsState {
    Creating,
    /// Set by `clone_ws` on the new doc — distinct from `Creating` so the UI can show a copy in
    /// progress rather than implying a from-scratch provision. The controller moves it to
    /// `Ready` same as `Creating` once the clone is materialized.
    Cloning,
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

pub const DEFAULT_WS_IMAGE: &str = "alpine:3.20";

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayerKind {
    Block,
    Stream,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineageEntry {
    pub kind: LayerKind,
    pub blob: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snap: Option<String>,
    pub sha256: String,
    /// LOCAL-ONLY, crash-recovery internal: true while this entry has been snapshotted+staged
    /// locally but not yet durable on the registry (blob uploaded, `CommitRecord` registered, ref
    /// moved) — a window that only exists mid-`push` or after one crashed partway through. The
    /// local `.lineage` pool file is the only place this state exists — a `CommitRecord` sent to
    /// the registry never carries it (always false on the wire; `push` clears it locally the
    /// moment the record lands). Defaults to false so old lineage files and every remote copy
    /// still parse.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unpushed: bool,
}

impl LineageEntry {
    /// Local pool string form, matching the POC `Entry` encoding: `s:{blob}:{sha}` for a
    /// stream layer, `b:{blob}:{snap}:{sha}` for a block layer, with a trailing `|u` when the
    /// entry is committed-but-not-pushed (see `unpushed`'s doc). The `|u` is appended, not
    /// woven into the `:`-separated fields, so a parser written before commit/push existed
    /// would simply see it as trailing garbage on `sha256` — there is no such parser left, but
    /// it kept the diff to `parse`/`encode` alone.
    pub fn encode(&self) -> String {
        let body = match self.kind {
            LayerKind::Stream => format!("s:{}:{}", self.blob, self.sha256),
            LayerKind::Block => format!(
                "b:{}:{}:{}",
                self.blob,
                self.snap.as_deref().unwrap_or(""),
                self.sha256
            ),
        };
        if self.unpushed { format!("{body}|u") } else { body }
    }

    /// `None` on a malformed line rather than a panic: the lineage file is plain text on a pool
    /// that can lose power mid-write, and a torn last line used to take the whole agent down
    /// through `Pool::lineage`'s `map`.
    pub fn parse(s: &str) -> Option<LineageEntry> {
        let (s, unpushed) = match s.strip_suffix("|u") {
            Some(rest) => (rest, true),
            None => (s, false),
        };
        let p: Vec<&str> = s.split(':').collect();
        match *p.first()? {
            "b" if p.len() >= 4 && !p[1].is_empty() && !p[3].is_empty() => Some(LineageEntry {
                kind: LayerKind::Block,
                blob: p[1].into(),
                snap: Some(p[2].into()),
                sha256: p[3].into(),
                unpushed,
            }),
            // `"s"` explicitly, not a catch-all. `encode` only ever writes `s:` or `b:`, so a line
            // starting with anything else is corruption — and a catch-all accepted `":::"` as a
            // stream layer with an empty blob and an empty hash, which is a lineage entry that
            // names nothing and would send the janitor looking for a blob called "".
            "s" if p.len() >= 3 && !p[1].is_empty() && !p[2].is_empty() => Some(LineageEntry {
                kind: LayerKind::Stream,
                blob: p[1].into(),
                snap: None,
                sha256: p[2].into(),
                unpushed,
            }),
            _ => None,
        }
    }

    /// Name of the local RO snapshot this entry materializes: the blob id for a stream
    /// layer, or the contained subvolume name for a block layer (the stream snapshot it
    /// materializes, so streams chain across the block boundary by received-UUID exactly as
    /// they would over the wire).
    pub fn snap_name(&self) -> &str {
        match self.kind {
            LayerKind::Stream => &self.blob,
            LayerKind::Block => self.snap.as_deref().unwrap_or(&self.blob),
        }
    }
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
    !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
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
    /// Same "copy in progress, not a from-scratch provision" distinction as `WsState::Cloning`.
    Cloning,
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
            &"x".repeat(64),
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

#[cfg(test)]
mod lineage_parse_tests {
    use super::LineageEntry;

    #[test]
    fn parse_survives_a_truncated_line() {
        // Every shape a power-loss mid-write can leave in the file. None of these may panic.
        for bad in ["", "b", "b:only-blob", "b:blob:snap", "s:", "s:blob", "|u", ":::"] {
            assert!(LineageEntry::parse(bad).is_none(), "{bad:?} must parse as None");
        }
        let good = "b:blob:snap:sha|u";
        let e = LineageEntry::parse(good).unwrap();
        assert!(e.unpushed);
        assert_eq!(e.encode(), good, "a good line still round-trips");
    }
}

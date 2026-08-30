//! Pure builders from the domain types to Kubernetes objects.
//!
//! No client, no I/O, no environment reads — every input arrives as an argument, which is what
//! makes the security-relevant paths here exhaustively testable.
//!
//! # Why local PersistentVolumes and not hostPath
//!
//! A workspace is a btrfs subvolume on one node, so the naive expression is a `hostPath` mount. It
//! was rejected for two reasons, both verified against the live cluster rather than assumed:
//!
//! * **Pod Security Admission forbids it.** `hostPath` is refused by BOTH `restricted` and
//!   `baseline` ("hostPath volumes (volume \"v\")"), so any namespace running user workloads would
//!   have to be `privileged` — surrendering namespace-level enforcement entirely for every pod,
//!   forever, to express one mount.
//! * **It made placement an assertion instead of a constraint** — until the pods here started
//!   carrying their own `nodeSelector` (see `placement`), so the scheduler enforces it the same
//!   way a `local` PV's `nodeAffinity` did. That objection no longer applies to the pod builders in
//!   this file; the PV-only half above still does.
//!
//! `persistentVolumeClaim` is an allowed volume type under `restricted`, so one static `local` PV
//! per volume gives the same bytes with none of that cost.
//!
//! # Why `baseline` and not `restricted`
//!
//! `restricted` additionally demands `runAsNonRoot`, and the default workspace image runs as root
//! (`nginx:alpine` fails with `container has runAsNonRoot and image will run as root`), as do the
//! common database images an environment is made of. `baseline` blocks what actually lets a
//! container escape — hostPath, privileged, hostNetwork/PID/IPC, dangerous capabilities — while
//! leaving root INSIDE the container, which a dev workspace genuinely needs. `restricted` is
//! recorded as warn+audit so the violations are visible without being fatal, and a namespace whose
//! images allow it can be raised to enforce individually.

use crate::crd::{PodResources, WorkspaceSpec};
use crate::model;
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EnvVar, HostPathVolumeSource, LimitRange, LimitRangeItem, LimitRangeSpec,
    KeyToPath, LocalObjectReference, LocalVolumeSource, Namespace, ObjectReference, SeccompProfile,
    NodeSelectorRequirement, NodeSelectorTerm, PersistentVolume, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, PersistentVolumeSpec, Pod,
    PodSpec, PodTemplateSpec, ResourceRequirements, Secret, SecretVolumeSource,
    SecurityContext, Service as CoreService,
    ServicePort, ServiceSpec, Toleration, Volume, VolumeMount, VolumeNodeAffinity,
    VolumeResourceRequirements,
};
use k8s_openapi::api::rbac::v1::{RoleBinding, RoleRef, Subject};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde_json::json;
use std::collections::BTreeMap;

pub const OWNER_LABEL: &str = "rustic-git.io/owner";
pub const KIND_LABEL: &str = "rustic-git.io/kind";
/// The team a workspace was made in, empty for personal. Same rule as the other two: a listing
/// view of `spec.team`, re-stamped by the controller, never authorization.
pub const TEAM_LABEL: &str = "rustic-git.io/team";
pub const SERVICE_LABEL: &str = "rustic-git.io/service";
/// The one StorageClass these PVs bind through. `no-provisioner` + `WaitForFirstConsumer`: nothing
/// is provisioned dynamically, and binding is deferred until a pod exists so the scheduler can
/// consider the PV's node affinity instead of binding first and discovering the conflict after.
pub const STORAGE_CLASS: &str = "rustic-git-local";
/// The container's writable layer and logs — NOT the tenant's data, which lives on their
/// PersistentVolume and is bounded by its own quota.
///
/// Unbounded, this is a node-wide denial of service available to any tenant: filling the kubelet's
/// disk taints the node `disk-pressure` and stops scheduling for every OTHER tenant on it. That is
/// not theoretical — it happened to this cluster from an ordinary build, and nothing in the
/// workload could have caused the kubelet to evict the offender instead of penalising the node.
/// With a limit the offending pod is evicted and its neighbours are untouched.
const EPHEMERAL_REQUEST: &str = "1Gi";
const EPHEMERAL_LIMIT: &str = "4Gi";

/// The label naming which workspace a pod belongs to. Load-bearing since workspaces share a
/// namespace: an attachment selects on it, so without it a grant would reach every workspace the
/// user owns.
pub const WORKSPACE_LABEL: &str = "rustic-git.io/workspace";

/// The PVC name for a volume. Per-volume, not fixed: a user's workspaces share one namespace, so a
/// single `live` claim would be one claim fought over by every workspace they own.
pub fn claim_name(id: &str) -> String {
    format!("live-{id}")
}

pub struct PodContext<'a> {
    /// The btrfs pool root on the node, e.g. `/wspool-prod`. Every volume builder needs it: a
    /// pod's `hostPath` is computed from it directly now, not resolved through a claim.
    pub pool: &'a str,
    pub node_name: &'a str,
    pub owner_ref: OwnerReference,
    /// The sandbox to run TENANT pods under, e.g. `gvisor`. `None` runs them on the host kernel.
    ///
    /// Opt-in, not defaulted, because a `runtimeClassName` naming a runtime the node has not got
    /// makes every pod fail to start — a cluster without gVisor installed must keep working. It is
    /// set from the agent's `WS_RUNTIME_CLASS`, so enabling it is a per-cluster decision made where
    /// the runtime is actually installed.
    ///
    /// Applies to tenant pods only. The controller itself must NOT be sandboxed: it drives btrfs
    /// against the host pool, which is precisely the host access a sandbox exists to remove.
    pub runtime_class: Option<&'a str>,
    /// The tagged image behind `model::DEFAULT_WS_IMAGE`, from the agent's `WS_DEFAULT_IMAGE`.
    pub default_image: &'a str,
}

pub(crate) fn labels(owner: &str, kind: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (OWNER_LABEL.to_string(), owner.to_string()),
        (KIND_LABEL.to_string(), kind.to_string()),
    ])
}

fn meta(name: &str, ns: Option<&str>, owner: &str, kind: &str, owner_ref: &OwnerReference) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: ns.map(str::to_string),
        labels: Some(labels(owner, kind)),
        // Deletion cascades through garbage collection rather than through cleanup code that can be
        // skipped, crash halfway, or be forgotten by a new code path.
        owner_references: Some(vec![owner_ref.clone()]),
        ..Default::default()
    }
}

/// `ws-{id}` / `env-{id}`, labelled for the policies that select it and for Pod Security Admission.
///
/// See the module docs for why this is `baseline` rather than `restricted`.
pub fn namespace(name: &str, owner: &str, kind: &str, owner_ref: Option<&OwnerReference>) -> Namespace {
    let mut l = labels(owner, kind);
    l.insert("pod-security.kubernetes.io/enforce".into(), "baseline".into());
    // Not fatal, but recorded: if an image ever CAN run non-root, these tell us so.
    l.insert("pod-security.kubernetes.io/warn".into(), "restricted".into());
    l.insert("pod-security.kubernetes.io/audit".into(), "restricted".into());
    Namespace {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(l),
            // `None` for a user's shared workspace namespace: an ownerReference here would make
            // deleting ONE workspace garbage-collect the namespace and every sibling workspace in
            // it. It is shared infrastructure — created on demand, left behind when empty. An
            // environment namespace does own its objects, because there it really is one-to-one.
            owner_references: owner_ref.map(|r| vec![r.clone()]),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The namespace's ceiling: no container in it may exceed the slot, and one that names no
/// resources at all gets the slot's values rather than none.
///
/// The pod specs this module builds already carry requests and limits, so this is not about them —
/// it is about everything else. A `LimitRange` is enforced by the API SERVER at admission, so it
/// holds for a pod created by any path: a future code path that forgets, a debug pod, an operator
/// with kubectl. Without it "every workspace is an M slot" is a property of one function rather
/// than of the namespace.
///
/// `max` is the slot's LIMIT, not its request: bursting to the limit is the point of the slot, and
/// exceeding it is what must be refused. Capacity is priced on the request (see
/// `PodResources::default`), which `defaultRequest` pins for anything that omits one.
pub fn limit_range(ns: &str, owner: &str, kind: &str, res: &PodResources, owner_ref: Option<&OwnerReference>) -> LimitRange {
    let item = LimitRangeItem {
        type_: "Container".to_string(),
        default: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_limit.clone())),
            ("memory".to_string(), Quantity(res.memory_limit.clone())),
        ])),
        default_request: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_request.clone())),
            ("memory".to_string(), Quantity(res.memory_request.clone())),
        ])),
        max: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_limit.clone())),
            ("memory".to_string(), Quantity(res.memory_limit.clone())),
        ])),
        ..Default::default()
    };
    LimitRange {
        metadata: ObjectMeta {
            name: Some("slot".to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(owner, kind)),
            owner_references: owner_ref.map(|r| vec![r.clone()]),
            ..Default::default()
        },
        spec: Some(LimitRangeSpec { limits: vec![item] }),
    }
}

/// The Secret name a namespace's pods pull private images with.
///
/// Fixed per namespace rather than per pod: a pull credential is scoped to the OWNER, not to one
/// workload, and one Secret per pod would be N copies of the same token to rotate.
pub const PULL_SECRET: &str = "registry-pull";

/// The Secret holding the owner's platform-issued git key, one per workspace namespace.
///
/// Per owner, not per workspace: the key IS the owner's git identity, so a copy per workspace would
/// be N copies of one credential to rotate.
pub const USER_KEY_SECRET: &str = "user-key";

/// Where that key is mounted. Deliberately not `~/.ssh`: workspace images bring their own user and
/// home directory, and `GIT_SSH_COMMAND` points at an absolute path that works whatever they are.
pub const USER_KEY_PATH: &str = "/etc/rustic-git/ssh";

/// The owner's private key as a namespace Secret. Written by the API tier, which holds `secrets`
/// only in namespaces the controller has vouched for — see `api_secret_binding`.
pub fn user_key_secret(owner: &str, namespace: &str, private_openssh: &str, m: &crate::api::OwnerMaterial) -> Secret {
    Secret {
        // No ownerReference: the key belongs to the OWNER, not to any one workspace, so deleting
        // the workspace that happened to trigger its creation must not take it with them.
        metadata: ObjectMeta {
            name: Some(USER_KEY_SECRET.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels(owner, "workspace")),
            ..Default::default()
        },
        // Both halves in ONE Secret: the private key the workspace pushes git with, and the
        // public keys sshd lets in. They are rewritten together, so splitting them would only add
        // a second object that can be half-written.
        string_data: Some(BTreeMap::from([
            ("id_ed25519".to_string(), private_openssh.to_string()),
            ("authorized_keys".to_string(), m.authorized_keys.clone()),
            // Read by git as its SYSTEM config (`GIT_CONFIG_SYSTEM`), so `~/.gitconfig` still
            // overrides it and a changed display name reaches running workspaces with the next
            // Secret rewrite, no restart. git's own escaping: a name with a quote is quoted.
            ("gitconfig".to_string(), gitconfig(&m.git_name, &m.git_email)),
        ])),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

fn gitconfig(name: &str, email: &str) -> String {
    let q = |v: &str| v.replace('\\', "\\\\").replace('"', "\\\"");
    format!("[user]\n\tname = \"{}\"\n\temail = \"{}\"\n", q(name), q(email))
}

/// Where sshd reads its config and host key. `/etc/ssh` is not a choice: `sshd` resolves relative
/// paths and its own defaults against it, and a config elsewhere still sends it looking here.
pub const SSHD_DIR: &str = "/etc/ssh";

/// Where sshd expects the owner's public keys. Unlike the git key this one CANNOT move: sshd
/// matches the file's path and mode against what its config declares, and nothing else reads it.
/// Who you are inside a workspace. Not root: sshd refuses root outright (`PermitRootLogin no`),
/// so a leaked key is a shell as an ordinary user, and everything a person writes lands owned
/// by an ordinary user. There is no sudo — root is `kubectl exec`, and installing software is
/// `spec.packages`. The uid is fixed so `~/workspaces/<name>` keeps its owner across pod restarts and
/// image changes.
pub const SSH_USER: &str = "kl";
/// Where workspace subvolumes are mounted: inside the home, one directory PER WORKSPACE named by
/// the workspace's NAME — the thing the person typed, not the id. Not a fixed `~/workspace`: the
/// home is shared across a person's workspaces, and tools that key their state on the working
/// directory (Claude Code's `~/.claude/projects/<path>`, opencode's sessions) would otherwise
/// see every workspace as the same project. `model::valid_ws_name` keeps the name a safe path
/// component. ponytail: two same-named workspaces in different teams of one person share the
/// directory name (and so that tool state) — names are unique per (owner, team), not per owner.
pub const WORKSPACES_DIR: &str = "/home/kl/workspaces";

pub fn workspace_dir(name: &str) -> String {
    format!("{WORKSPACES_DIR}/{name}")
}

/// The seeder's own view of the same volume. It runs before the home exists, so it mounts the
/// subvolume at a fixed path of its own rather than under `~`.
const SEED_DIR: &str = "/workspace";
/// Where the owner's persistent home is mounted; every `workspace_dir(name)` is inside it.
/// Everything under here except `workspaces/` is the same in every workspace the person opens
/// on this node.
pub const HOME_DIR: &str = "/home/kl";
/// The claim every workspace pod in a namespace mounts at `HOME_DIR`. A fixed name: there is one
/// home per (owner, namespace), so an id would only repeat what the namespace already says.
pub const HOME_CLAIM: &str = "home";
/// What inside a home is a nested subvolume rather than a directory: package caches. btrfs `send`
/// skips a nested subvolume and the home's qgroup does not count it, so these never upload and
/// never eat the quota. ONE list, read by the create path and the restore path in the engine — two
/// lists would drift and a cache would come back as a plain directory that the next push carries.
/// A person who wants something else excluded runs `btrfs subvolume create` themselves; that is the
/// documented escape hatch, not a UI. The editors' remote servers are here too: the default image
/// exists to run VS Code's, which is 300 MB+ per version and would otherwise be most of the 2 GB
/// quota and of every five-minute push, and it re-downloads itself on a fresh node anyway.
pub const HOME_LOCAL_DIRS: [&str; 6] =
    [".cache", ".npm", ".cargo/registry", ".local/share/pnpm", ".vscode-server", ".cursor-server"];
pub const SSH_UID: i64 = 1000;
const SSH_HOME: &str = "/home/kl/.ssh";
const AUTHORIZED_KEYS_PATH: &str = "/home/kl/.ssh/authorized_keys";

/// The per-workspace host key Secret's name.
pub fn ws_ssh_secret_name(id: &str) -> String {
    format!("ws-ssh-{id}")
}

/// sshd's whole configuration, generated so `sshd_config` and the mounts that satisfy it cannot
/// drift apart.
///
/// `PermitRootLogin no` and `AllowUsers kl`: the only way in is a key the owner registered, and
/// it opens a shell as `kl`, never as the root the container itself runs as. `StrictModes no` because `authorized_keys` is a Secret mount, and a
/// Secret mount is a world-writable tmpfs (`drwxrwxrwt`) — sshd would refuse every key in it as
/// "bad ownership or modes" otherwise; the mount is read-only, so the mode guards nothing.
/// `ClientAliveInterval 30` is not a nicety — Cloudflare idles a
/// WebSocket after 100s, and the tunnel is the whole data path.
pub fn sshd_config(name: &str) -> String {
    let set_env = format!("SetEnv {}", login_env(name).iter().map(|e| format!("\"{}={}\"", e.name, e.value.as_deref().unwrap_or_default())).collect::<Vec<_>>().join(" "));
    format!(
        "Port 22\n\
         HostKey {SSHD_DIR}/ssh_host_ed25519_key\n\
         PermitRootLogin no\n\
         AllowUsers {SSH_USER}\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         PubkeyAuthentication yes\n\
         AuthorizedKeysFile {AUTHORIZED_KEYS_PATH}\n\
         StrictModes no\n\
         {}\n\
         AllowTcpForwarding yes\n\
         X11Forwarding no\n\
         ClientAliveInterval 30\n\
         Subsystem sftp {}/libexec/sftp-server\n",
        // sshd hands a login NONE of the container's environment — the same PATH, git key and git
        // identity the pod's entrypoint sees have to be restated here, or `git push` over ssh
        // has no key and a Nix tool is "not found". ONE directive: sshd keeps only the first
        // `SetEnv` line it meets (`sshd -T` showed a single variable when they were split), so
        // every variable rides on the same line, each quoted because values hold spaces.
        set_env,
        crate::packages::PROFILE_LINK
    )
}

/// The environment a workspace shell sees, whether it is the image's entrypoint or an ssh login:
/// the Nix profile on PATH, git's key and identity. ONE list, because sshd does not inherit the
/// container's environment and two lists would drift.
fn login_env(name: &str) -> Vec<EnvVar> {
    let var = |n: &str, v: String| EnvVar { name: n.into(), value: Some(v), ..Default::default() };
    vec![
        git_ssh_command(),
        // Which workspace this shell is in: the platform rc files cd into it and the prompt
        // names it. Per pod, which is why sshd's SetEnv is generated per workspace.
        var("KL_WORKSPACE", workspace_dir(name)),
        var("KL_WORKSPACE_NAME", name.to_string()),
        var("GIT_CONFIG_SYSTEM", format!("{USER_KEY_PATH}/gitconfig")),
        // ponytail: an image with a non-standard PATH loses it; read it from the image config
        // via the registry if that ever matters.
        var("PATH", crate::packages::path_env(None)),
        var("NIX_PROFILE", crate::packages::PROFILE_LINK.into()),
        // zsh's rc lives under `~/.config` with fish's, so the persistent home carries every shell's
        // config in one tree; without this zsh reads `~/.zshrc` and finds nothing.
        var("ZDOTDIR", format!("{HOME_DIR}/.config/zsh")),
        var("MANPATH", format!("{}/share/man:", crate::packages::PROFILE_LINK)),
        var("XDG_DATA_DIRS", format!("{}/share:/usr/local/share:/usr/share", crate::packages::PROFILE_LINK)),
    ]
}

/// What the default image runs before sshd, as root, on every container start. The image
/// (Dockerfile `workspace` stage) already carries the accounts, the chroot dir and the greeting;
/// this is only what depends on the mounts: seeding the rc files, owning the volume, exec.
/// `~/workspaces` is this pod's own emptyDir, mounted over the shared home, so the workspace
/// mount point inside it never lands in the home and no pod lists a sibling's; root only has to
/// hand that emptyDir to `kl` (a mount point cannot be a symlink, so root may chown it).
/// The platform's shell config lives in `/etc` (container filesystem, rewritten every start,
/// never inside the person's home): an interactive login lands in the workspace, and starship
/// shows the directory, not `user@pod` — inside the workspace that directory IS its name — unless
/// the person keeps their own `~/.config/starship.toml`, which then wins. Nix's zsh reads `/etc/zshrc`, its fish
/// `/etc/fish/conf.d/*.fish`.
///
/// The shell is zsh from the Nix profile (with fish alongside and starship for the prompt), so
/// `WS_BASE_PACKAGES` must keep `zsh fish starship`; the profile is mounted before this runs.
/// The two apk packages are for VS Code Remote-SSH: its Alpine server ships a musl `node`
/// that still dlopens libstdc++ and libgcc_s, which stock alpine lacks — without them every
/// connect downloads the server and dies with "Error relocating … libstdc++". Nix cannot
/// supply them (its libstdc++ is glibc-linked). Best effort: no network at boot is not a
/// reason to refuse the shell.
/// `adduser -D` writes `!` as the password, which sshd reads as "account locked" and refuses
/// even a valid key; `*` is "no password" and is not locked. `~/workspaces/<id>` is chowned every
/// start because the seeder clones it as root and a restore can bring back files owned by
/// anyone. `exec` so sshd is pid 1 and gets the kubelet's TERM.
///
/// Root touches exactly ONE path under the home: `chown $H`, and `$H` is a mountpoint, which
/// cannot be a symlink. Everything below it — the mkdirs, the rc seeds — runs as `kl` via `su`,
/// because the home is now persistent and the person owns every byte of it between starts:
/// `mv ~/.config x; ln -s /etc ~/.config` would otherwise make the next start `chown` and write
/// through `/etc` as root, and the container keeps CHOWN/DAC_OVERRIDE on a writable rootfs. The
/// seed runs from a heredoc on `su`'s stdin (busybox `su -c` would need the printf quoting nested
/// a second time), with `set -e` of its own so a failed seed still stops the pod.
/// ponytail: `chown -R` walks the whole volume on every start; fine for source trees. `$H` is the
/// persistent home PV and the rc files are seeded only if absent, so a person's own edits survive
/// a restart and a new workspace alike; `~/workspaces/<id>` is a mount point inside it that the
/// kubelet makes, which is why nothing here mkdirs it.
fn prelude(name: &str) -> String {
    let workspace_dir = workspace_dir(name);
    let profile = crate::packages::PROFILE_LINK;
    let path = crate::packages::path_env(None);
    format!(
        "set -e\n\
         H=/home/{SSH_USER}\n\
         chown {SSH_UID}:{SSH_UID} $H $H/workspaces\n\
         mkdir -p /etc/fish/conf.d\n\
         printf '%s\\n' '[[ -o interactive ]] || return 0' '[ \"$PWD\" = \"$HOME\" ] && [ -d \"$KL_WORKSPACE\" ] && cd \"$KL_WORKSPACE\"' '[ -e \"$HOME/.config/starship.toml\" ] || export STARSHIP_CONFIG=/etc/starship.toml' > /etc/zshrc\n\
         printf '%s\\n' 'status is-interactive; or exit' 'if test \"$PWD\" = \"$HOME\" -a -d \"$KL_WORKSPACE\"; cd \"$KL_WORKSPACE\"; end' 'test -e \"$HOME/.config/starship.toml\"; or set -gx STARSHIP_CONFIG /etc/starship.toml' > /etc/fish/conf.d/kl.fish\n\
         printf '%s\\n' 'format = \"$directory$git_branch$git_status$cmd_duration$line_break$character\"' > /etc/starship.toml\n\
         su {SSH_USER} -s /bin/sh <<'SEED'\n\
         set -e\n\
         export PATH={path}\n\
         H=/home/{SSH_USER}\n\
         mkdir -p $H/.config/fish $H/.config/zsh\n\
         [ -e $H/.config/zsh/.zshrc ] || printf 'export PATH={path}\\neval \"$(dircolors -b)\"\\nzstyle \":completion:*\" list-colors \"${{(s.:.)LS_COLORS}}\"\\nalias ls=\"ls --color=auto\" grep=\"grep --color=auto\"\\neval \"$(starship init zsh)\"\\n' > $H/.config/zsh/.zshrc\n\
         [ -e $H/.config/fish/config.fish ] || printf 'set -gx PATH {path}\\nset -gx LS_COLORS (dircolors -b | string match -r \"LS_COLORS=.([^\\047]*)\")[2]\\nalias ls=\"ls --color=auto\"\\nalias grep=\"grep --color=auto\"\\nstarship init fish | source\\n' > $H/.config/fish/config.fish\n\
         SEED\n\
         chown -Rh {SSH_UID}:{SSH_UID} {workspace_dir}\n\
         exec {profile}/bin/sshd -D -e -f {SSHD_DIR}/sshd_config\n"
    )
}

/// This workspace's ed25519 host key, generated once by the node that owns it.
///
/// Per workspace and owned BY the workspace: it is the identity users pin in `known_hosts`, so it
/// must survive pod recreation (hence a Secret, not a file on the subvolume) and die with the
/// workspace (hence the ownerReference — a clone is a different host and gets its own).
pub fn ws_ssh_secret(
    id: &str,
    name: &str,
    namespace: &str,
    owner: &str,
    owner_ref: &OwnerReference,
    private_openssh: &str,
    public_line: &str,
) -> Secret {
    Secret {
        metadata: meta(&ws_ssh_secret_name(id), Some(namespace), owner, "workspace", owner_ref),
        string_data: Some(BTreeMap::from([
            ("ssh_host_ed25519_key".to_string(), private_openssh.to_string()),
            ("ssh_host_ed25519_key.pub".to_string(), public_line.to_string()),
            ("sshd_config".to_string(), sshd_config(name)),
        ])),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

/// `/etc/ssh` for the pod. The private key needs 0400 — sshd exits rather than read a host key
/// anything else can — while the config it reads at the same time is not a secret.
fn ws_ssh_volume(id: &str) -> Volume {
    Volume {
        name: "ws-ssh".to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(ws_ssh_secret_name(id)),
            default_mode: Some(0o400),
            items: Some(vec![
                KeyToPath { key: "ssh_host_ed25519_key".into(), path: "ssh_host_ed25519_key".into(), mode: None },
                KeyToPath { key: "ssh_host_ed25519_key.pub".into(), path: "ssh_host_ed25519_key.pub".into(), mode: None },
                KeyToPath { key: "sshd_config".into(), path: "sshd_config".into(), mode: Some(0o444) },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The owner's public keys, from the SAME Secret the git key lives in — the API rewrites both
/// halves together. A second volume rather than a second mount of `user-key` because sshd refuses
/// an `authorized_keys` wider than 0600, and the mode is a property of the volume.
///
/// `items` names ONLY the public half: the whole Secret at `/home/kl/.ssh` would put the owner's
/// private git key where ssh picks identities up by default, and it already has a home at
/// `USER_KEY_PATH`.
///
/// Mounted as a DIRECTORY, never as a `subPath` of this file. A `subPath` of an OPTIONAL Secret
/// wedges the pod in ContainerCreating with "failed to prepare subPath" when the Secret is not
/// there yet, and a `subPath` mount never sees later writes — so a key added in the UI would need
/// a pod recreate to take effect. As a directory the kubelet fills it in when the Secret appears
/// and refreshes it when the API rewrites it, which is what "sshd reads the file per login" needs.
fn authorized_keys_volume() -> Volume {
    Volume {
        name: "authorized-keys".to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(USER_KEY_SECRET.to_string()),
            items: Some(vec![KeyToPath {
                key: "authorized_keys".into(),
                path: "authorized_keys".into(),
                mode: Some(0o444),
            }]),
            // Same reason as `user_key_volume`: the API writes this after the namespace exists, so
            // a workspace can be scheduled before its keys are there. A pod that waits for it is a
            // workspace that never starts because its owner has registered no key.
            optional: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn user_key_volume(required: bool) -> Volume {
    Volume {
        name: "user-key".to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(USER_KEY_SECRET.to_string()),
            // 0444, deliberately: the file is root's (the kubelet writes it) and git runs as `kl`.
            // ssh's "unprotected private key" refusal only fires for a file the CALLER owns, so
            // root's key at 0444 is one `kl` may use. Not `fsGroup` — that re-modes EVERY Secret
            // in the pod, including the sshd host key, which sshd then refuses as too open.
            // World-readable inside a single-person pod is the pod's own boundary, not a wider one.
            default_mode: Some(0o444),
            // The API writes this AFTER the controller has made the namespace, so a workspace can
            // be scheduled before its key exists. Optional means the pod starts anyway and the
            // kubelet fills the mount in when the Secret shows up, instead of the pod sitting
            // Pending until then. A SEEDED workspace cannot tolerate that: the init container
            // clones with this key, and an absent one would start a pod that clones nothing and
            // then reports Ready.
            optional: Some(!required),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Let the API write Secrets in THIS namespace, and nowhere else.
///
/// The API needs to place a short-lived git token for a workspace being seeded from a repository.
/// Granting `secrets: create` cluster-wide to achieve that would hand it every Secret in the
/// cluster, the agent's own credentials included — so the permission is bound per namespace, by the
/// controller, as it creates each workspace namespace.
///
/// The controller can only issue this grant because it holds `bind` on exactly this ClusterRole:
/// Kubernetes otherwise refuses to let a subject hand out permissions it does not itself have, and
/// the alternative (giving the controller cluster-wide secret access so it can delegate a slice of
/// it) is the thing being avoided.
///
/// `owner_ref` is the OwnerBinding that vouched for the namespace, when one did: the grant is
/// per (owner, node) and so shares that lifetime. It is never a Workspace or an Environment — the
/// namespace is shared by every workspace the user owns, so deleting one must not revoke the grant
/// for its siblings.
pub fn api_secret_binding(
    ns: &str,
    owner: &str,
    api_service_account: &str,
    api_namespace: &str,
    owner_ref: Option<&OwnerReference>,
) -> RoleBinding {
    secret_binding(ns, owner, "api-secrets", "rustic-git-api-secrets", api_service_account, api_namespace, owner_ref)
}

/// The agent's OWN per-namespace Secret grant, for the `ws-ssh-{id}` host keys it reads and
/// creates. The alternative was `secrets: get, create` cluster-wide on the agent's ClusterRole —
/// which included `rustic-git-jwt` and the api's credentials in `kube-system`, so one compromised
/// node could read every tenant's signing key. Bound here, in the namespace this same reconciler
/// just made, so the grant exists before the first workspace's `ensure_ssh` needs it
/// (`namespace_ready` gates that). The ClusterRole is in `deploy/k3s/agent-rbac.yaml`; the
/// admission policy beside it pins which roles this binding may name.
pub const AGENT_SERVICE_ACCOUNT: &str = "rustic-git-agent";
pub const AGENT_NAMESPACE: &str = "kube-system";
pub fn agent_secret_binding(ns: &str, owner: &str, owner_ref: &OwnerReference) -> RoleBinding {
    secret_binding(ns, owner, "agent-secrets", "rustic-git-agent-ws-secrets", AGENT_SERVICE_ACCOUNT, AGENT_NAMESPACE, Some(owner_ref))
}

fn secret_binding(
    ns: &str,
    owner: &str,
    name: &str,
    role: &str,
    service_account: &str,
    sa_namespace: &str,
    owner_ref: Option<&OwnerReference>,
) -> RoleBinding {
    RoleBinding {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(owner, "workspace")),
            owner_references: owner_ref.map(|r| vec![r.clone()]),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: role.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: service_account.to_string(),
            namespace: Some(sa_namespace.to_string()),
            ..Default::default()
        }]),
    }
}

/// The PV name for a volume id. Cluster-scoped, so it carries the id rather than living in a
/// namespace that already implies it.
pub fn pv_name(id: &str) -> String {
    format!("pv-{id}")
}

/// A statically provisioned `local` PV over one host path — a workspace's btrfs subvolume, or
/// the shared read-only `/nix` store.
///
/// `Retain`, never `Delete`: the reclaim policy decides what happens to a user's data when their
/// claim goes away, and `Delete` would hand that decision to the kubelet. Reclaiming a subvolume is
/// the controller's job, done deliberately, after the finalizer says the bytes are gone.
#[allow(clippy::too_many_arguments)]
pub fn local_pv(
    name: &str,
    ns: &str,
    claim: &str,
    host_path: &str,
    access_mode: &str,
    capacity_gb: u64,
    owner: &str,
    ctx: &PodContext,
) -> PersistentVolume {
    PersistentVolume {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(labels(owner, "volume")),
            owner_references: Some(vec![ctx.owner_ref.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeSpec {
            capacity: Some(BTreeMap::from([("storage".to_string(), Quantity(format!("{capacity_gb}Gi")))])),
            access_modes: Some(vec![access_mode.to_string()]),
            persistent_volume_reclaim_policy: Some("Retain".to_string()),
            storage_class_name: Some(STORAGE_CLASS.to_string()),
            local: Some(LocalVolumeSource { path: host_path.to_string(), ..Default::default() }),
            // Pre-bound from THIS side, not by `volumeName` on the claim. Every `nix-*` volume
            // points at the same `/nix` and the `live-*` ones differ only by path, so each must be
            // pinned to exactly one claim — but a claim that names its volume opts out of
            // `WaitForFirstConsumer`, and binding then waits on the PV controller's own schedule
            // (measured: 0.4 s to 12.3 s for the same shape). A `claimRef` pins it just as
            // exactly and binds in well under a second.
            //
            // No uid: a claim deleted and recreated gets a new one, and matching by namespace and
            // name is what lets the replacement bind.
            claim_ref: Some(ObjectReference {
                namespace: Some(ns.to_string()),
                name: Some(claim.to_string()),
                ..Default::default()
            }),
            // This is what replaces naming a node on the pod: the scheduler will only place a pod
            // using this claim onto this node, and says so when it cannot.
            node_affinity: Some(VolumeNodeAffinity {
                required: Some(k8s_openapi::api::core::v1::NodeSelector {
                    node_selector_terms: vec![NodeSelectorTerm {
                        match_expressions: Some(vec![NodeSelectorRequirement {
                            key: "kubernetes.io/hostname".to_string(),
                            operator: "In".to_string(),
                            values: Some(vec![ctx.node_name.to_string()]),
                        }]),
                        ..Default::default()
                    }],
                }),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The claim binding a namespace to one PV.
///
/// It names no volume: the pairing is expressed from the PV side, by the `claimRef` `local_pv`
/// writes. Same exclusivity — one namespace, one claim — but a claim without `volume_name` still
/// takes the storage class's `WaitForFirstConsumer` path instead of waiting on the PV controller's
/// resync.
pub fn claim(
    ns: &str,
    name: &str,
    access_mode: &str,
    capacity_gb: u64,
    owner: &str,
    owner_ref: &OwnerReference,
) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: meta(name, Some(ns), owner, "volume", owner_ref),
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec![access_mode.to_string()]),
            storage_class_name: Some(STORAGE_CLASS.to_string()),
            resources: Some(VolumeResourceRequirements {
                requests: Some(BTreeMap::from([("storage".to_string(), Quantity(format!("{capacity_gb}Gi")))])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The host Nix store, exposed to a workspace the same way its subvolume is: a local PV names the
/// host path, the pod names a claim. A local PV binds to exactly one claim, so it is one per
/// workspace even though every one of them points at the same `/nix` — PV objects are cheap and
/// the alternative is a hostPath, which PSA `baseline` forbids for good reason. Capacity is a
/// required field with no meaning for it (shared and read-only), hence the flat 1Gi callers pass.
pub const NIX_ROOT: &str = "/nix";

pub fn nix_pv_name(id: &str) -> String { format!("nix-{id}") }
pub fn nix_claim_name(id: &str) -> String { format!("nix-{id}") }

/// The local PV behind `HOME_CLAIM` in `ns`. Per NAMESPACE rather than per owner: a local PV
/// binds to exactly one claim, and an owner with workspaces in two teams has two namespaces that
/// each need their own claim on the one host path. Cluster-scoped, so the namespace is in the name.
pub fn home_pv_name(ns: &str) -> String {
    format!("home-{ns}")
}

/// The host path backing a volume's live subvolume.
pub fn live_path(pool: &str, id: &str) -> String { format!("{pool}/vol/{id}/live") }

/// The claim every workspace pod in a namespace mounts to get its `/etc/resolv.conf`.
///
/// One claim per namespace, like `HOME_CLAIM` and unlike the Nix store's per-workspace pair: a
/// local PV binds to exactly one claim, but a claim may be mounted by every pod in its namespace,
/// and the per-workspace part is the `subPath`. One writer (the agent) and read-only consumers is
/// what makes sharing it safe — two writers over one host path would be a silent data race.
pub const ATTACH_CLAIM: &str = "attach";

/// The local PV behind `ATTACH_CLAIM` in `ns`. Cluster-scoped, so the namespace is in the name.
pub fn attach_pv_name(ns: &str) -> String {
    format!("attach-{ns}")
}

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

/// A typed host directory. `Directory` rather than the default: an untyped `hostPath` CREATES a
/// missing path as an empty directory, so a pod that lands where its subvolume is not would start
/// with a blank home and no error. Typed, the kubelet refuses the mount and the pod says so.
///
/// Read-only intent is not expressed here — a `hostPath` volume has no such field — it lives,
/// enforced, on the `VolumeMount`s that reference it.
fn host_dir(name: &str, path: String) -> Volume {
    Volume {
        name: name.to_string(),
        host_path: Some(HostPathVolumeSource { path, type_: Some("Directory".into()) }),
        ..Default::default()
    }
}

/// The store, read-only. Mounted at its root because the profile lives under it too; the
/// individual mounts below pick the two subdirectories the pod may see.
fn nix_volume() -> Volume {
    host_dir("nix", NIX_ROOT.to_string())
}

/// This workspace's rendered `resolv.conf`. A FILE, and mounted as one: the agent rewrites it in
/// place precisely because the pod holds the inode.
fn attach_volume(pool: &str, ws_id: &str) -> Volume {
    Volume {
        name: "attach".to_string(),
        host_path: Some(HostPathVolumeSource { path: attach_file(pool, ws_id), type_: Some("File".into()) }),
        ..Default::default()
    }
}

fn quantities(res: &PodResources) -> ResourceRequirements {
    // Requests AND limits on every user container: requests are what the scheduler packs against,
    // limits are what stops one workspace eating a node its neighbours share.
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_request.clone())),
            ("memory".to_string(), Quantity(res.memory_request.clone())),
            ("ephemeral-storage".to_string(), Quantity(EPHEMERAL_REQUEST.to_string())),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), Quantity(res.cpu_limit.clone())),
            ("memory".to_string(), Quantity(res.memory_limit.clone())),
            ("ephemeral-storage".to_string(), Quantity(EPHEMERAL_LIMIT.to_string())),
        ])),
        ..Default::default()
    }
}

/// What `baseline` does not enforce but we can still apply per container.
///
/// `run_as_non_root` is deliberately absent — see the module docs: forcing it would break the
/// zero-configuration default image and most database images an environment is built from.
fn hardened() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        // The kernel's default syscall filter. Not required by `baseline` — which is why it was
        // missing — but it is free, needs no change to the image, and is the single largest
        // reduction in kernel attack surface available to a container that must run as root.
        // Both the NSA/CISA hardening guidance and PSA `restricted` ask for it.
        seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".to_string(), localhost_profile: None }),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            // Drop everything, then add back only what an ordinary image needs to INITIALISE.
            // `drop: ALL` alone is not deployable for images users actually bring: the default
            // workspace image dies at startup with
            //   nginx: [emerg] chown("/var/cache/nginx/client_temp", 101) failed (1: Operation not permitted)
            // because its entrypoint runs as root, chowns its cache dirs and drops to the nginx
            // user — the same shape postgres, mongo and most official images use. Observed on the
            // cluster, not theorised.
            //
            // Every one of these is on Pod Security Admission `baseline`'s allowed-add list, so the
            // namespace still rejects the dangerous ones (SYS_ADMIN, NET_RAW, SYS_PTRACE and the
            // rest) — which is the property that actually matters. This is "the container runtime's
            // ordinary default, stated explicitly" rather than a widening of it.
            add: Some(
                // SYS_CHROOT is for sshd: its privilege-separation monitor chroots the
                // unauthenticated child into /var/empty and refuses every login without it.
                [
                    "CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID", "NET_BIND_SERVICE",
                    "SYS_CHROOT",
                ]
                    .iter()
                    .map(|c| c.to_string())
                    .collect(),
            ),
        }),
        privileged: Some(false),
        ..Default::default()
    }
}

/// The owner's persistent home. `home_id` is `crd::home_volume_name(owner)` — always via the
/// function, never formatted here.
fn home_volume(pool: &str, home_id: &str) -> Volume {
    host_dir("home", live_path(pool, home_id))
}

/// An emptyDir for `WORKSPACES_DIR`. Per pod on purpose — see the mount's comment.
fn workspaces_volume() -> Volume {
    Volume { name: "workspaces".to_string(), empty_dir: Some(Default::default()), ..Default::default() }
}

/// The workspace's own subvolume.
fn live_volume(pool: &str, id: &str) -> Volume {
    host_dir("live", live_path(pool, id))
}

/// Keep the pod on its role's nodes and on the node holding its subvolume, and tolerate that
/// role's taint.
///
/// Two selectors, two jobs: the role key says "session pods run on session nodes" (and a
/// single-node install may carry both role labels), the hostname pins this pod to the node holding
/// its subvolume. That pin used to come from the PV's `nodeAffinity`; with the volumes mounted from
/// the host there is no PV to carry it, and an unpinned pod would mount an empty directory on the
/// wrong node. The toleration is not optional: the label without it schedules nothing.
fn placement(spec: &mut PodSpec, role: &str, node: &str) {
    // One label KEY per role (`rustic-git.io/session`, `rustic-git.io/env`) rather than one shared
    // key with the role as its value. A label key holds a single value, so `role=session` and
    // `role=env` are mutually exclusive and no node could ever serve both — which made a
    // single-node install impossible, and produced an unschedulable pod whose data was on one node
    // and whose selector demanded another:
    //   1 node(s) didn't match PersistentVolume's node affinity
    //   1 node(s) didn't match Pod's node affinity/selector
    // Separate keys let a small or CI cluster put both roles on one box and a large one keep them
    // apart, with no change to this code.
    spec.node_selector = Some(BTreeMap::from([
        (format!("rustic-git.io/{role}"), "true".to_string()),
        ("kubernetes.io/hostname".to_string(), node.to_string()),
    ]));
    spec.tolerations = Some(vec![Toleration {
        key: Some(format!("rustic-git.io/{role}")),
        operator: Some("Exists".to_string()),
        effect: Some("NoSchedule".to_string()),
        ..Default::default()
    }]);
    // A user workload has no business talking to the API server.
    spec.automount_service_account_token = Some(false);
}

/// The one definition of `GIT_SSH_COMMAND`, shared by the workspace container and the seeder. Two
/// copies of an ssh invocation that must agree is two invocations that will not.
///
/// `IdentitiesOnly` stops ssh offering an agent key first and getting refused for too many
/// attempts; `accept-new` trusts the host on first sight, which is the only workable answer when
/// nothing here has a known_hosts file.
fn git_ssh_command() -> EnvVar {
    EnvVar {
        name: "GIT_SSH_COMMAND".to_string(),
        value: Some(format!(
            "ssh -i {USER_KEY_PATH}/id_ed25519 -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
        )),
        ..Default::default()
    }
}

/// The container that seeds a `gitRepo` workspace, or `None` for any other source.
///
/// It runs INSIDE the workspace, over SSH, as the owner, with the platform key the pod already
/// mounts. That is the whole reason the credential Secret is gone: there is no third party to mint
/// a token for, and the git tier already decides what this key may read.
///
/// `repo` is `owner/name`, never a URL, and the host comes from the agent's env — a caller cannot
/// point this at an arbitrary endpoint, which would be an egress and SSRF primitive available to
/// anyone who can create a workspace. Both halves are validated HERE and not only at the API,
/// because this is the last place before the value becomes an ssh argv: anything that writes a
/// Volume by another path (a restored backup, kubectl) reaches this function and not that handler.
/// `Err` is a permanent failure, never a retry — a bad name never becomes a good one.
///
/// ponytail: `--depth 1` shallow, so `git log` in the workspace shows one commit; deepen on demand
/// if anyone asks for the history they did not ask to clone.
pub fn git_init_container(
    source: &crate::crd::VolumeSource,
    init_image: &str,
    ssh_host: &str,
    ssh_port: &str,
) -> Result<Option<Container>, String> {
    let crate::crd::VolumeSource::GitRepo { repo, branch } = source else { return Ok(None) };
    let ok = repo.split_once('/').is_some_and(|(o, n)| {
        rustic_git_storage::store::valid_owner(o) && rustic_git_storage::store::valid_segment(n)
    });
    if !ok {
        return Err(format!("source repo {repo:?} is not owner/name"));
    }
    // A leading `-` is an option, not a branch: `git clone --branch -upload-pack=…` is arbitrary
    // command execution on this pod. `..` is refused for the same reason `valid_segment` refuses it.
    if branch.is_empty() || branch.starts_with('-') || branch.contains("..") {
        return Err(format!("source branch {branch:?} is not a branch name"));
    }
    let url = if ssh_port.is_empty() {
        format!("ssh://git@{ssh_host}/{repo}.git")
    } else {
        format!("ssh://git@{ssh_host}:{ssh_port}/{repo}.git")
    };
    Ok(Some(Container {
        name: "git-seed".to_string(),
        image: Some(init_image.to_string()),
        // The empty-dir check is what makes this idempotent: a pod restart, a node reboot or a
        // second reconcile must never clone over work the user has done.
        command: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("set -e; [ \"$(ls -A {SEED_DIR})\" ] || git clone --depth 1 --single-branch --branch \"$BRANCH\" -- \"$URL\" {SEED_DIR}")
                .to_string(),
        ]),
        env: Some(vec![
            EnvVar { name: "URL".to_string(), value: Some(url), ..Default::default() },
            EnvVar { name: "BRANCH".to_string(), value: Some(branch.clone()), ..Default::default() },
            git_ssh_command(),
        ]),
        volume_mounts: Some(vec![
            VolumeMount { name: "live".to_string(), mount_path: SEED_DIR.to_string(), ..Default::default() },
            VolumeMount {
                name: "user-key".to_string(),
                mount_path: USER_KEY_PATH.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        // ponytail: `hardened()` sets no `run_as_user`, so the seed lands as the INIT IMAGE's user
        // (root for `alpine/git`). A workspace image running as a non-root user would find its
        // clone unwritable; the fix then is an explicit `runAsUser` on both containers, from the
        // image's own uid.
        security_context: Some(hardened()),
        ..Default::default()
    }))
}

/// The workspace's one pod.
pub fn workspace_pod(spec: &WorkspaceSpec, id: &str, ctx: &PodContext, init: Option<Container>) -> Pod {
    // ssh is a feature of the DEFAULT image only: a user image brings its own entrypoint, and we
    // cannot replace it with sshd without breaking whatever it was built to run.
    let default_image = crate::model::is_default_image(&spec.image);
    let mut ssh_mounts = vec![];
    if default_image {
        ssh_mounts = vec![
            VolumeMount { name: "ws-ssh".into(), mount_path: SSHD_DIR.into(), read_only: Some(true), ..Default::default() },
            VolumeMount { name: "authorized-keys".into(), mount_path: SSH_HOME.into(), read_only: Some(true), ..Default::default() },
        ];
    }
    let mut pod_spec = PodSpec {
        containers: vec![Container {
            name: "workspace".to_string(),
            image: Some(if default_image { ctx.default_image.to_string() } else { spec.image.clone() }),
            // Only the default image is told what to run: it is a bare alpine, and sshd from its
            // Nix profile is both what keeps it alive and how people get in. A user's own image
            // keeps its entrypoint — we cannot know what it expects to run, and overriding it
            // would break every image that starts a daemon.
            // Everything a bare alpine lacks for sshd and a login is made at start (see
            // `prelude`) rather than baked into an image, so the default image stays stock alpine.
            command: default_image.then(|| vec!["/bin/sh".to_string(), "-c".to_string(), prelude(&spec.name)]),
            ports: default_image.then(|| {
                vec![ContainerPort { container_port: 22, name: Some("ssh".into()), ..Default::default() }]
            }),
            volume_mounts: Some(vec![
                // Listed before the workspace mount for the reader; the kubelet orders by path
                // depth and `workspace_dir(name)` is under `HOME_DIR`, so the order is implied either way.
                VolumeMount { name: "home".to_string(), mount_path: HOME_DIR.to_string(), ..Default::default() },
                // This pod's own `~/workspaces`, over the shared home: the workspace's mount point
                // is made inside it, so it never appears in the home and no sibling pod lists it.
                VolumeMount { name: "workspaces".to_string(), mount_path: WORKSPACES_DIR.to_string(), ..Default::default() },
                VolumeMount {
                    name: "live".to_string(),
                    mount_path: workspace_dir(&spec.name),
                    ..Default::default()
                },
                VolumeMount {
                    name: "user-key".to_string(),
                    mount_path: USER_KEY_PATH.to_string(),
                    read_only: Some(true),
                    ..Default::default()
                },
                // The store and THIS workspace's profile only — `/nix` itself holds every other
                // workspace's profile and the daemon socket, so the pod never sees its root.
                VolumeMount { name: "nix".to_string(), mount_path: "/nix/store".to_string(), sub_path: Some("store".to_string()), read_only: Some(true), ..Default::default() },
                VolumeMount { name: "nix".to_string(), mount_path: crate::packages::PROFILE_MOUNT.to_string(), sub_path: Some(format!("var/rustic/profiles/{id}")), read_only: Some(true), ..Default::default() },
                // Mounting over `/etc/resolv.conf` is the only way to change a live pod's DNS —
                // `dnsConfig` is immutable once it is running. The volume IS the file now, so no
                // subPath: the agent rewrites it in place and the pod sees the change.
                VolumeMount {
                    name: "attach".into(),
                    mount_path: "/etc/resolv.conf".into(),
                    read_only: Some(true),
                    ..Default::default()
                },
            ].into_iter().chain(ssh_mounts).collect()),
            // So `git` in the workspace uses the platform key and commits as the owner without
            // anyone configuring it. The same list feeds sshd's `SetEnv`.
            env: Some(login_env(&spec.name)),
            resources: Some(quantities(&spec.resources)),
            security_context: Some(hardened()),
            ..Default::default()
        }],
        // Required, not optional, for a seeded workspace: the init container cannot clone without
        // the key.
        volumes: Some({
            let mut v = vec![
                home_volume(ctx.pool, &crate::crd::home_volume_name(&spec.owner)),
                workspaces_volume(),
                live_volume(ctx.pool, id),
                nix_volume(),
                attach_volume(ctx.pool, id),
                user_key_volume(init.is_some()),
            ];
            if default_image {
                v.extend([ws_ssh_volume(id), authorized_keys_volume()]);
            }
            v
        }),
        init_containers: init.map(|c| vec![c]),
        // Optional by design: the kubelet ignores a named pull secret that does not exist, so a
        // public image keeps working in a namespace that has never been given a credential.
        image_pull_secrets: Some(vec![LocalObjectReference { name: PULL_SECRET.to_string() }]),
        // What `--restart unless-stopped` became: stopping is expressed by deleting the pod, not by
        // a policy the kubelet interprets.
        restart_policy: Some("Always".to_string()),
        // What the prompt shows (`kl@ws`), not the generated pod name — the id is in `kl ws list`.
        hostname: Some("ws".to_string()),
        runtime_class_name: ctx.runtime_class.map(str::to_string),
        ..Default::default()
    };
    placement(&mut pod_spec, "session", ctx.node_name);
    let mut m = meta(
        id,
        Some(&crate::crd::ws_namespace(&spec.owner, &spec.team)),
        &spec.owner,
        "workspace",
        &ctx.owner_ref,
    );
    // Which workspace this pod IS. Siblings share the namespace, so an attachment grant that named
    // only the namespace would reach all of them; this label is what keeps it to one.
    if let Some(l) = m.labels.as_mut() {
        l.insert(WORKSPACE_LABEL.to_string(), id.to_string());
    }
    Pod { metadata: m, spec: Some(pod_spec), ..Default::default() }
}

/// The env unit from the capacity model: 4 GB limit, packed at 1.5x oversubscription, so the
/// request is 4 GB / 1.5 = 2730Mi. Requesting 512Mi against a 4Gi limit was 8x oversubscription,
/// not 1.5x — five times more services on a node than the model prices, every one of them able to
/// claim memory that is not there.
///
/// CPU stays small deliberately: envs are memory-bound and idle services need almost none, so
/// packing is decided by memory alone.
///
/// One definition, used by both the Deployment and the namespace's `LimitRange`. Two copies of a
/// number that must agree is two numbers that will not.
pub fn env_unit_resources() -> PodResources {
    PodResources {
        cpu_request: "250m".into(),
        cpu_limit: "2".into(),
        memory_request: "2730Mi".into(),
        memory_limit: "4Gi".into(),
    }
}

/// One Deployment per service in an environment.
///
/// **Every mount goes through `validate_mount` here.** An environment has ONE volume, and each
/// declared mount is a folder inside it, expressed as a `subPath` on the shared hostPath. Kubernetes
/// rejects `..` in a subPath itself, but this does not lean on that: a folder is validated as a
/// single safe segment before it is ever formatted into one.
pub fn service_statefulset(
    svc: &model::Service,
    env_id: &str,
    owner: &str,
    ctx: &PodContext,
) -> Result<StatefulSet, String> {
    // The API checked this at create; re-checked here because this is the last point before the
    // values become object names, and it also covers an Environment written by any other path.
    model::validate_service(svc)?;
    let mut mounts = Vec::new();
    for m in &svc.mounts {
        mounts.push(VolumeMount {
            name: "live".to_string(),
            mount_path: m.path.clone(),
            sub_path: Some(format!("volumes/{}", m.folder)),
            ..Default::default()
        });
    }

    let mut sel = labels(owner, "environment");
    sel.insert(SERVICE_LABEL.to_string(), svc.name.clone());

    let mut pod_spec = PodSpec {
        containers: vec![Container {
            name: svc.name.clone(),
            image: Some(svc.image.clone()),
            command: (!svc.command.is_empty()).then(|| svc.command.clone()),
            // Sorted: `env` is a HashMap, and a template whose variable order differs from the
            // last apply is a new revision — a rollout nobody asked for on every reconcile.
            env: Some(
                svc.env
                    .iter()
                    .collect::<std::collections::BTreeMap<_, _>>()
                    .into_iter()
                    .map(|(k, v)| EnvVar {
                        name: k.clone(),
                        value: Some(v.clone()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ports: Some(
                svc.ports
                    .iter()
                    .map(|p| ContainerPort {
                        container_port: *p as i32,
                        ..Default::default()
                    })
                    .collect(),
            ),
            volume_mounts: (!mounts.is_empty()).then_some(mounts),
            resources: Some(quantities(&env_unit_resources())),
            security_context: Some(hardened()),
            ..Default::default()
        }],
        volumes: Some(vec![live_volume(ctx.pool, env_id)]),
        // An environment's services are the likeliest place a private image appears — they are
        // whatever the user named, not our default.
        image_pull_secrets: Some(vec![LocalObjectReference { name: PULL_SECRET.to_string() }]),
        runtime_class_name: ctx.runtime_class.map(str::to_string),
        ..Default::default()
    };
    placement(&mut pod_spec, "env", ctx.node_name);

    Ok(StatefulSet {
        metadata: meta(
            &svc.name,
            Some(&crate::crd::env_namespace(env_id)),
            owner,
            "environment",
            &ctx.owner_ref,
        ),
        // A StatefulSet, not a Deployment, and the reason is its one-pod-per-ordinal guarantee:
        // `db-0` is never created until the previous `db-0` is fully gone — on updates AND on
        // node failures — where a Deployment surges a second pod first. Every service mounts the
        // environment's one subvolume, and two mongods on one WiredTiger directory is how a real
        // environment got a torn block. Availability is not what this object is for.
        spec: Some(StatefulSetSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(sel.clone()),
                ..Default::default()
            },
            // The ClusterIP Service of the same name: what makes `db:27017` resolve. Not headless,
            // and nothing here needs the per-ordinal `db-0.db` name.
            service_name: Some(svc.name.clone()),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(sel),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// The ClusterIP that gives a service its DNS name — what makes `mongodb://db:27017` resolve from a
/// sibling service, and from an attached workspace on another node.
pub fn service_clusterip(
    svc: &model::Service,
    env_id: &str,
    owner: &str,
    owner_ref: &OwnerReference,
) -> CoreService {
    let mut sel = labels(owner, "environment");
    sel.insert(SERVICE_LABEL.to_string(), svc.name.clone());
    CoreService {
        metadata: meta(
            &svc.name,
            Some(&crate::crd::env_namespace(env_id)),
            owner,
            "environment",
            owner_ref,
        ),
        spec: Some(ServiceSpec {
            selector: Some(sel),
            ports: Some(
                svc.ports
                    .iter()
                    .map(|p| ServicePort {
                        name: Some(format!("p{p}")),
                        port: *p as i32,
                        target_port: Some(IntOrString::Int(*p as i32)),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The specs below are static JSON rather than nested `Some(vec![…])` structs: they never branch,
/// and the shape a reviewer has to check against the Kubernetes docs is the shape they read here.
fn policy(name: &str, ns: &str, owner: &str, owner_ref: &OwnerReference, spec: serde_json::Value) -> NetworkPolicy {
    NetworkPolicy {
        metadata: meta(name, Some(ns), owner, "policy", owner_ref),
        spec: Some(serde_json::from_value(spec).expect("static NetworkPolicy spec")),
    }
}

/// The three policies every namespace gets: deny everything, allow DNS out, allow the namespace to
/// talk to itself.
///
/// Generated rather than rendered from YAML so there is exactly one definition of the isolation
/// rule. Order does not matter — NetworkPolicies are additive, and the default-deny is expressed by
/// selecting every pod with no rules rather than by precedence.
pub fn default_policies(ns: &str, owner: &str, owner_ref: &OwnerReference) -> Vec<NetworkPolicy> {
    vec![
        policy(
            "default-deny",
            ns,
            owner,
            owner_ref,
            json!({ "podSelector": {}, "policyTypes": ["Ingress", "Egress"] }),
        ),
        policy(
            "allow-dns",
            ns,
            owner,
            owner_ref,
            // To CoreDNS specifically, by its namespace's well-known label. Without this rule
            // every lookup fails, which is the most common way a default-deny namespace looks
            // like "the network is broken".
            json!({
                "podSelector": {},
                "policyTypes": ["Egress"],
                "egress": [{
                    "to": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "kube-system" } } }],
                    "ports": [
                        { "protocol": "UDP", "port": 53 },
                        { "protocol": "TCP", "port": 53 },
                    ],
                }],
            }),
        ),
        allow_internet_egress(ns, owner, owner_ref),
        policy(
            "allow-same-namespace",
            ns,
            owner,
            owner_ref,
            // An environment's services must reach each other — that is what an environment IS.
            json!({
                "podSelector": {},
                "policyTypes": ["Ingress", "Egress"],
                "ingress": [{ "from": [{ "podSelector": {} }] }],
                "egress": [{ "to": [{ "podSelector": {} }] }],
            }),
        ),
    ]
}

/// Everything a tenant must NEVER reach on egress, as CIDRs excluded from the public internet.
///
/// `169.254.0.0/16` is the one that matters most: `169.254.169.254` is the cloud instance metadata
/// service, and on Azure it hands out the NODE's managed identity to anything that asks. A tenant
/// that reaches it holds the node's cloud credentials, which is a full escape from the cluster, not
/// merely from the namespace.
///
/// The private ranges cover the pod network (10.42/16), the service network (10.43/16) and the
/// node subnet (10.60/16) without this code having to know them — and blocking all of RFC 1918
/// rather than the three specific ranges means a cluster that renumbers does not silently open a
/// hole. Nothing a dev workspace legitimately fetches lives on a private address.
const CLUSTER_INTERNALS: [&str; 4] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16"];

/// Egress to the public internet, and nothing private.
///
/// A workspace has to reach npm, crates.io, GitHub — a dev environment that cannot fetch a
/// dependency is not one. But "allow egress" written the obvious way (`0.0.0.0/0`) also opens the
/// metadata service and every internal address, which is why this is an allow-list with holes
/// punched OUT rather than a permit-all.
///
/// Additive with the rest: `allow-dns` still permits CoreDNS (inside 10/8, excluded here) and
/// `allow-same-namespace` still permits siblings, because NetworkPolicies union.
pub fn allow_internet_egress(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        "allow-internet-egress",
        ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Egress"],
            "egress": [{ "to": [{ "ipBlock": { "cidr": "0.0.0.0/0", "except": CLUSTER_INTERNALS } }] }],
        }),
    )
}

/// The namespace `deploy/k3s/gateway.yaml` puts the gateway in. Its own, not `kube-system`: the
/// gateway is the internet-facing process, and the namespace used to be chosen only so this
/// policy's selector could name it — a `namespaceSelector` names any namespace just as well.
pub const GATEWAY_NAMESPACE: &str = "rustic-git-system";

/// The one hole in a workspace namespace's default-deny ingress: port 22, from the gateway pods in
/// `GATEWAY_NAMESPACE` and nothing else.
///
/// Both selectors sit in ONE peer, which is an AND. Written as two peers it would be an OR, and
/// any pod in the cluster — including another tenant's workspace — could reach every sshd by
/// labelling itself `app=rustic-git-gateway`.
pub fn allow_gateway_ingress(ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        "allow-gateway-ssh",
        ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": GATEWAY_NAMESPACE } },
                    "podSelector": { "matchLabels": { "app": "rustic-git-gateway" } },
                }],
                "ports": [{ "protocol": "TCP", "port": 22 }],
            }],
        }),
    )
}

/// Both halves of an attachment grant share this name, one in each namespace, so a detach can
/// delete them by name without a lookup.
pub fn attach_policy_name(ws_id: &str) -> String {
    format!("attach-{ws_id}")
}

/// Lets one workspace pod reach the environment's namespace.
///
/// Egress needs its own rule because `allow_internet_egress` deliberately excludes RFC 1918, so
/// the pod network is unreachable by default — the environment's ClusterIP included. Selects the
/// POD by `WORKSPACE_LABEL`, never the namespace: an owner's workspaces share a namespace, so a
/// namespace-wide grant would open every workspace they own to this one environment.
pub fn attach_egress(ws_ns: &str, ws_id: &str, env_ns: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &attach_policy_name(ws_id),
        ws_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": env_ns } } }],
            }],
        }),
    )
}

/// Lets the environment accept that one workspace pod.
///
/// Namespace and pod selector sit in ONE element of `from`, which ANDs them: as two elements they
/// would OR, admitting every pod in the workspace namespace and every pod anywhere carrying that
/// label — including another owner's workspace that happens to share the same id.
pub fn attach_ingress(env_ns: &str, ws_ns: &str, ws_id: &str, owner: &str, owner_ref: &OwnerReference) -> NetworkPolicy {
    policy(
        &attach_policy_name(ws_id),
        env_ns,
        owner,
        owner_ref,
        json!({
            "podSelector": {},
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{
                    "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": ws_ns } },
                    "podSelector": { "matchLabels": { WORKSPACE_LABEL: ws_id } },
                }],
            }],
        }),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::DesiredState;
    use crate::model::Mount;

    const AGENT_RESOLV: &str = "search kube-system.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n";

    /// Unattached: the workspace's own namespace leads, and everything the agent's file said about
    /// nameserver, ndots and the node's suffix is carried through untouched.
    #[test]
    fn an_unattached_resolv_conf_is_what_kubelet_would_have_written() {
        let got = resolv_conf(AGENT_RESOLV, "ws-acme", None);
        assert_eq!(
            got,
            "search ws-acme.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
        );
    }

    /// Attached: the environment's namespace goes FIRST, so a name the environment defines wins
    /// over one in the workspace's own namespace.
    #[test]
    fn an_attached_resolv_conf_searches_the_environment_first() {
        let got = resolv_conf(AGENT_RESOLV, "ws-acme", Some("env-abc"));
        assert_eq!(
            got,
            "search env-abc.svc.cluster.local ws-acme.svc.cluster.local svc.cluster.local cluster.local node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
        );
    }

    /// A cluster started with a non-default `--cluster-domain` must not get `cluster.local`
    /// search entries — the domain is derived from the template, never assumed.
    #[test]
    fn a_non_default_cluster_domain_is_derived_from_the_template() {
        let template = "search kube-system.svc.cluster.internal svc.cluster.internal cluster.internal node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n";
        let got = resolv_conf(template, "ws-acme", Some("env-abc"));
        assert_eq!(
            got,
            "search env-abc.svc.cluster.internal ws-acme.svc.cluster.internal svc.cluster.internal cluster.internal node.example.net\nnameserver 10.43.0.10\noptions ndots:5\n"
        );
    }

    /// A template with no search line at all still yields a usable file rather than a malformed one.
    #[test]
    fn a_template_without_a_search_line_gains_one() {
        let got = resolv_conf("nameserver 10.43.0.10\n", "ws-acme", Some("env-abc"));
        assert_eq!(
            got,
            "search env-abc.svc.cluster.local ws-acme.svc.cluster.local svc.cluster.local cluster.local\nnameserver 10.43.0.10\n"
        );
    }

    fn owner_ref() -> OwnerReference {
        OwnerReference {
            api_version: "rustic-git.io/v1alpha1".into(),
            kind: "Volume".into(),
            name: "vol-1".into(),
            uid: "uid-1".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    /// Storage is mounted from the node, not claimed. Every source carries an explicit `type`: an
    /// untyped hostPath creates a missing path as an empty directory, which is a wiped workspace
    /// rather than a failed mount.
    #[test]
    fn every_volume_is_a_typed_host_path() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        assert!(
            vols.iter().all(|v| v.persistent_volume_claim.is_none()),
            "no pod claims a PVC any more"
        );
        for v in vols.iter().filter(|v| v.host_path.is_some()) {
            let h = v.host_path.as_ref().unwrap();
            // `DirectoryOrCreate` passes a presence check just as well as `Directory` does, and is
            // exactly the value this test exists to catch: it creates a missing path as an empty
            // directory, which is a silently wiped workspace on the wrong node.
            let want = if v.name == "attach" { "File" } else { "Directory" };
            assert_eq!(h.type_.as_deref(), Some(want), "hostPath {:?} must be typed {want}", v.name);
            assert!(h.path.starts_with('/'), "hostPath {:?} must be absolute", v.name);
        }
    }

    /// The three per-workspace paths are the ones `ensure_storage` used to hand the PV builder.
    #[test]
    fn the_host_paths_are_the_ones_the_pv_layer_used() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let path = |n: &str| {
            vols.iter().find(|v| v.name == n).unwrap_or_else(|| panic!("no {n} volume"))
                .host_path.as_ref().unwrap().path.clone()
        };
        assert_eq!(path("live"), live_path(ctx().pool, "ws-1"));
        assert_eq!(path("nix"), NIX_ROOT);
        assert_eq!(path("attach"), attach_file(ctx().pool, "ws-1"));
    }

    /// Placement is the pod's own now that no PV carries node affinity, and it is ADDED to the
    /// role selector rather than replacing it.
    #[test]
    fn the_pod_selects_its_node_by_hostname() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let s = p.spec.unwrap();
        let sel = s.node_selector.expect("a node selector");
        assert_eq!(sel.get("kubernetes.io/hostname").map(String::as_str), Some("session-0"));
        assert_eq!(sel.get("rustic-git.io/session").map(String::as_str), Some("true"));
        assert!(s.node_name.is_none(), "the scheduler still places the pod");
    }

    /// The env pod gets the same placement fix as the workspace pod: `service_statefulset` has no
    /// PV to carry `nodeAffinity` any more either.
    #[test]
    fn the_service_pod_selects_its_node_by_hostname() {
        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx()).unwrap();
        let s = d.spec.unwrap().template.spec.unwrap();
        let sel = s.node_selector.expect("a node selector");
        assert_eq!(sel.get("kubernetes.io/hostname").map(String::as_str), Some("session-0"));
        assert_eq!(sel.get("rustic-git.io/env").map(String::as_str), Some("true"));
        assert!(s.node_name.is_none(), "the scheduler still places the pod");
    }

    fn ctx() -> PodContext<'static> {
        PodContext { pool: "/mnt/wspool", node_name: "session-0", owner_ref: owner_ref(), runtime_class: Some("gvisor"), default_image: "ghcr.io/kloudlite/rustic-git-workspace:deadbeef" }
    }

    fn svc(folder: &str, path: &str) -> model::Service {
        model::Service {
            name: "web".into(),
            image: "nginx".into(),
            command: vec![],
            env: Default::default(),
            mounts: vec![Mount { folder: folder.into(), path: path.into() }],
            ports: vec![80],
        }
    }

    fn ws_spec() -> WorkspaceSpec {
        WorkspaceSpec {
            team: String::new(),
            owner: "alice".into(),
            name: "dev".into(),
            region: "centralindia".into(),
            image: "nginx:alpine".into(),
            storage: Some(crate::crd::WorkspaceStorage { quota_gb: 10, source: None }),
            desired_state: DesiredState::Running,
            resources: PodResources::default(),
            packages: vec![],
            attached_environment: None,
        }
    }

    /// Per NAMESPACE, like the home claim: a local PV binds to one claim, but one claim serves
    /// every pod in the namespace. The per-workspace part is the subPath, not the object.
    #[test]
    fn the_attach_claim_is_one_per_namespace() {
        assert_eq!(attach_pv_name("ws-acme"), "attach-ws-acme");
        assert_eq!(ATTACH_CLAIM, "attach");
        assert_eq!(attach_root("/pool"), "/pool/attach");
        assert_eq!(attach_file("/pool", "ws-1"), "/pool/attach/ws-1/resolv.conf");
    }

    /// The mount is what makes attachment live: the agent rewrites the host file and the running
    /// pod sees it. Read-only so the person in the workspace cannot point their own DNS elsewhere.
    #[test]
    fn a_workspace_pod_mounts_its_own_resolv_conf() {
        let spec = ws_spec();
        let pod = workspace_pod(&spec, "ws-1", &ctx(), None);
        let podspec = pod.spec.unwrap();
        let vol = podspec.volumes.unwrap().into_iter().find(|v| v.name == "attach").expect("attach volume");
        let h = vol.host_path.unwrap();
        assert_eq!(h.path, attach_file(ctx().pool, "ws-1"));
        assert_eq!(h.type_.as_deref(), Some("File"));
        let mount = podspec.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.mount_path == "/etc/resolv.conf")
            .expect("resolv.conf mount");
        assert!(mount.sub_path.is_none(), "the volume IS the file now");
        assert_eq!(mount.read_only, Some(true));
    }

    #[test]
    fn the_user_key_secret_carries_authorized_keys() {
        let m = crate::api::OwnerMaterial {
            authorized_keys: "ssh-ed25519 AAAA alice@laptop".into(),
            git_name: "Alice \"Al\" Liddell".into(),
            git_email: "alice@example.com".into(),
        };
        let s = user_key_secret("alice", "ws-alice", "PRIVATE", &m);
        let data = s.string_data.unwrap();
        assert_eq!(data["id_ed25519"], "PRIVATE");
        // sshd inside the workspace reads this file; it is the whole of "who may ssh in".
        assert_eq!(data["authorized_keys"], "ssh-ed25519 AAAA alice@laptop");
        // A quote in a name must not end git's string early.
        assert_eq!(data["gitconfig"], "[user]\n\tname = \"Alice \\\"Al\\\" Liddell\"\n\temail = \"alice@example.com\"\n");
    }

    #[test]
    fn a_service_is_a_statefulset_with_a_stable_template() {
        let mut s = svc("data", "/data");
        s.env = [("Z", "1"), ("A", "2"), ("M", "3")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let d = service_statefulset(&s, "env-1", "team", &ctx()).unwrap();
        let spec = d.spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(spec.service_name.as_deref(), Some("web"), "the ClusterIP Service of the same name");
        let names: Vec<_> = spec.template.spec.unwrap().containers[0].env.as_ref().unwrap().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["A", "M", "Z"], "a stable template is what keeps the ReplicaSet from changing under a database");
    }

    #[test]
    fn a_service_deployment_refuses_a_mount_that_escapes_the_subvolume() {
        let ctx = ctx();
        let ok = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx).unwrap();
        let mounts = ok.spec.as_ref().unwrap().template.spec.as_ref().unwrap().containers[0]
            .volume_mounts
            .as_ref()
            .unwrap();
        assert_eq!(mounts[0].sub_path.as_deref(), Some("volumes/data"));
        assert_eq!(mounts[0].name, "live", "a mount is a subPath of the env's one volume");

        // The C1 payload: `{"folder": "/", "path": "/host"}`. Kubernetes rejects `..` in a subPath
        // itself, but this must not lean on that — the segment is validated before it is formatted.
        for bad in ["/", "..", "a/b", "", "../../root/.ssh", "a:b"] {
            assert!(
                service_statefulset(&svc(bad, "/host"), "env-1", "team", &ctx).is_err(),
                "folder {bad:?} must be refused"
            );
        }
        assert!(service_statefulset(&svc("data", "/data:/etc"), "env-1", "team", &ctx).is_err());
        assert!(service_statefulset(&svc("data", "relative"), "env-1", "team", &ctx).is_err());
    }

    /// Tenants share a node, so they share its kernel. A sandbox runtime puts a userspace kernel
    /// between the tenant and the host one — the only thing here that turns a kernel exploit from
    /// a host compromise into a sandbox escape.
    ///
    /// Opt-in: a `runtimeClassName` naming a runtime the node lacks makes every pod fail to start,
    /// so a cluster without gVisor installed must keep working.
    #[test]
    fn tenant_pods_run_under_the_sandbox_when_one_is_configured() {
        let ctx = ctx(); // runtime_class: Some("gvisor")
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx, None);
        assert_eq!(p.spec.unwrap().runtime_class_name.as_deref(), Some("gvisor"));

        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx).unwrap();
        assert_eq!(
            d.spec.unwrap().template.spec.unwrap().runtime_class_name.as_deref(),
            Some("gvisor"),
            "an environment's services are tenant workloads too"
        );

        // Unset means the host kernel, not a broken pod.
        let bare = PodContext { pool: "/mnt/wspool", node_name: "session-0", owner_ref: owner_ref(), runtime_class: None, default_image: "ghcr.io/kloudlite/rustic-git-workspace:deadbeef" };
        assert!(workspace_pod(&ws_spec(), "ws-1", &bare, None).spec.unwrap().runtime_class_name.is_none());
    }

    #[test]
    fn no_pod_this_module_builds_uses_a_claim() {
        // A PVC binds through the StorageClass and a local PV; the pods mount the host directly
        // now, so a PVC reappearing here would mean a builder regressed to the old shape.
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        for v in p.spec.unwrap().volumes.unwrap() {
            assert!(v.persistent_volume_claim.is_none(), "workspace pod must mount a hostPath, not a claim");
            // The key is a Secret, `~/workspaces` is a per-pod emptyDir (baseline allows it);
            // everything else is the workspace's data, which is a hostPath.
            assert!(v.host_path.is_some() || v.secret.is_some() || v.empty_dir.is_some());
        }
        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx()).unwrap();
        for v in d.spec.unwrap().template.spec.unwrap().volumes.unwrap() {
            assert!(v.persistent_volume_claim.is_none(), "service pod must mount a hostPath, not a claim");
        }
    }

    #[test]
    fn the_volume_pins_the_node_and_never_deletes_the_data() {
        let pv = local_pv(&pv_name("ws-1"), "ws-alice", &claim_name("ws-1"), &live_path(ctx().pool, "ws-1"), "ReadWriteOnce", 20, "alice", &ctx());
        let spec = pv.spec.unwrap();
        assert_eq!(spec.local.as_ref().unwrap().path, "/mnt/wspool/vol/ws-1/live");
        // Retain, never Delete: reclaiming a user's subvolume is a deliberate controller action,
        // not something the kubelet does when a claim goes away.
        assert_eq!(spec.persistent_volume_reclaim_policy.as_deref(), Some("Retain"));

        // The scheduler enforces placement from this, which is why the pod no longer names a node.
        let term = &spec.node_affinity.unwrap().required.unwrap().node_selector_terms[0];
        let e = &term.match_expressions.as_ref().unwrap()[0];
        assert_eq!(e.key, "kubernetes.io/hostname");
        assert_eq!(e.values.as_deref(), Some(&["session-0".to_string()][..]));

        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        assert!(
            p.spec.unwrap().node_name.is_none(),
            "naming a node here would make placement an assertion again"
        );
    }

    #[test]
    fn a_claim_binds_to_exactly_one_named_volume() {
        let c = claim("ws-alice", &claim_name("ws-1"), "ReadWriteOnce", 20, "alice", &owner_ref());
        assert_eq!(c.metadata.name.as_deref(), Some("live-ws-1"), "siblings share a namespace");
        let s = c.spec.unwrap();
        assert_eq!(s.storage_class_name.as_deref(), Some(STORAGE_CLASS));
        // The exclusivity is unchanged, only the side it is written on: without a pairing the
        // claim would bind to whichever PV of this class fits — which, for per-workspace storage,
        // means somebody else's data — and the PV's `claimRef` is now what pins it.
        assert!(s.volume_name.is_none(), "the claim must not name its volume");
        let pv = local_pv(&pv_name("ws-1"), "ws-alice", &claim_name("ws-1"), &live_path(ctx().pool, "ws-1"), "ReadWriteOnce", 20, "alice", &ctx());
        let cr = pv.spec.unwrap().claim_ref.unwrap();
        assert_eq!((cr.namespace.as_deref(), cr.name.as_deref()), (Some("ws-alice"), Some("live-ws-1")));
    }

    /// Pre-binding moved to the PV: a claim naming its volume opts out of WaitForFirstConsumer,
    /// which is what made binding take anywhere from 0.4 s to 12.3 s.
    #[test]
    fn the_volume_names_its_claim_and_the_claim_names_no_volume() {
        let pv = local_pv("live-ws-1", "ws-acme", "live-ws-1", "/pool/vol/ws-1/live", "ReadWriteOnce", 20, "acme", &ctx());
        let cr = pv.spec.unwrap().claim_ref.expect("the PV names its claim");
        assert_eq!(cr.namespace.as_deref(), Some("ws-acme"));
        assert_eq!(cr.name.as_deref(), Some("live-ws-1"));
        assert!(cr.uid.is_none(), "no uid: a recreated claim must still match by name");

        let c = claim("ws-acme", "live-ws-1", "ReadWriteOnce", 20, "acme", &owner_ref());
        assert!(c.spec.unwrap().volume_name.is_none(), "the claim must not name its volume");
    }

    #[test]
    fn a_user_pod_cannot_reach_the_api_server_or_escalate() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let s = p.spec.unwrap();
        assert_eq!(s.automount_service_account_token, Some(false));
        assert_eq!(s.restart_policy.as_deref(), Some("Always"));
        // A key per role, not a shared key with the role as its value: a node can then carry both
        // and a single-node install works.
        assert_eq!(
            s.node_selector.as_ref().unwrap().get("rustic-git.io/session").map(String::as_str),
            Some("true")
        );
        // The label without the toleration schedules nothing.
        assert_eq!(s.tolerations.as_ref().unwrap()[0].key.as_deref(), Some("rustic-git.io/session"));

        let c = &s.containers[0];
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        // The kernel's default syscall filter. `baseline` does not demand it, so nothing else
        // would catch its removal.
        assert_eq!(sc.seccomp_profile.as_ref().unwrap().type_, "RuntimeDefault");
        let caps = sc.capabilities.as_ref().unwrap();
        assert_eq!(caps.drop.as_deref(), Some(&["ALL".to_string()][..]));
        // Only the init set, and every entry must be one PSA `baseline` permits — an add outside
        // that list is rejected by the namespace at admission, which is a pod that never starts.
        const BASELINE_ALLOWED: [&str; 13] = [
            "AUDIT_WRITE", "CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "KILL", "MKNOD",
            "NET_BIND_SERVICE", "SETFCAP", "SETGID", "SETPCAP", "SETUID", "SYS_CHROOT",
        ];
        for c in caps.add.as_deref().unwrap_or_default() {
            assert!(BASELINE_ALLOWED.contains(&c.as_str()), "{c} is not allowed under baseline");
        }

        let r = c.resources.as_ref().unwrap();
        assert!(r.requests.as_ref().unwrap().contains_key("memory"));
        assert!(r.limits.as_ref().unwrap().contains_key("memory"));
        assert!(r.requests.as_ref().unwrap().contains_key("cpu"));
        assert!(r.limits.as_ref().unwrap().contains_key("cpu"));
        // Without this a tenant can fill the node's disk, taint it `disk-pressure` and stop
        // scheduling for every other tenant on it — a node-wide denial of service from one pod.
        assert!(
            r.limits.as_ref().unwrap().contains_key("ephemeral-storage"),
            "an unbounded writable layer is a node-wide DoS"
        );
    }

    /// The capacity model prices a node by how many workspaces and services fit on it, and what
    /// fits is decided by the REQUEST, not the limit. These numbers are therefore a pricing input,
    /// not a tuning knob — drifting them silently changes what a workspace costs.
    ///
    /// "M session" in the model is a workspace. On a 32-OCPU / 128 GB session node at 94% usable
    /// memory: 120 GB ÷ 4 GB = 30 workspaces, needing 30 × 2 = 60 vCPU of the 64 available.
    #[test]
    fn pod_requests_match_the_capacity_model() {
        let r = PodResources::default();
        assert_eq!(r.memory_request, "4Gi", "M workspace guarantee is 4 GB");
        assert_eq!(r.memory_limit, "8Gi", "M workspace limit is 8 GB");
        assert_eq!(r.cpu_request, "2", "2 vCPU guaranteed, and deliberately not oversubscribed");
        assert_eq!(r.cpu_limit, "4");

        // An environment service: 4 GB limit packed at 1.5x oversubscription.
        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx()).unwrap();
        let res = d.spec.unwrap().template.spec.unwrap().containers[0].resources.clone().unwrap();
        let req = res.requests.unwrap();
        let lim = res.limits.unwrap();
        assert_eq!(lim.get("memory").unwrap().0, "4Gi");
        assert_eq!(req.get("memory").unwrap().0, "2730Mi", "4 GB / 1.5x oversubscription");
    }

    /// The slot has to be enforced by the NAMESPACE, not just by the function that builds pods.
    /// A `LimitRange` is applied at admission, so it holds for a pod created by any path — a future
    /// code path that forgets, a debug pod, an operator with kubectl.
    #[test]
    fn the_namespace_refuses_anything_larger_than_its_slot() {
        let lr = limit_range("ws-alice", "alice", "workspace", &PodResources::default(), None);
        let item = &lr.spec.unwrap().limits[0];
        assert_eq!(item.type_, "Container");

        // max is the slot's LIMIT: bursting to it is the point, exceeding it is refused.
        let max = item.max.as_ref().unwrap();
        assert_eq!(max.get("memory").unwrap().0, "8Gi");
        assert_eq!(max.get("cpu").unwrap().0, "4");

        // defaultRequest is what capacity is priced on, for anything that names no request.
        let dr = item.default_request.as_ref().unwrap();
        assert_eq!(dr.get("memory").unwrap().0, "4Gi");
        assert_eq!(dr.get("cpu").unwrap().0, "2");

        // Shared user namespace: no ownerReference, or deleting one workspace drops the ceiling
        // for every sibling.
        assert!(lr.metadata.owner_references.is_none());

        // The environment ceiling matches the unit the Deployment actually requests.
        let env = limit_range("env-1", "team", "environment", &env_unit_resources(), Some(&owner_ref()));
        let env_item = &env.spec.unwrap().limits[0];
        assert_eq!(env_item.max.as_ref().unwrap().get("memory").unwrap().0, "4Gi");
        assert_eq!(env_item.default_request.as_ref().unwrap().get("memory").unwrap().0, "2730Mi");
    }

    /// The API's Secret access must be namespaced, never cluster-wide: a cluster-wide grant would
    /// include every Secret in the cluster, the agent's own credentials among them.
    #[test]
    fn the_api_secret_grant_is_scoped_to_one_namespace() {
        let rb = api_secret_binding("ws-alice", "alice", "rustic-git-api", "kube-system", None);
        assert_eq!(rb.metadata.namespace.as_deref(), Some("ws-alice"), "a RoleBinding, not a ClusterRoleBinding");
        assert_eq!(rb.role_ref.name, "rustic-git-api-secrets");
        assert_eq!(rb.role_ref.kind, "ClusterRole", "the rules are shared; only the scope is per namespace");
        let sub = &rb.subjects.unwrap()[0];
        assert_eq!(sub.name, "rustic-git-api");
        assert_eq!(sub.namespace.as_deref(), Some("kube-system"));
        // Shared user namespace: deleting one workspace must not revoke the grant for its siblings.
        assert!(rb.metadata.owner_references.is_none());
        // The OwnerBinding, and only it, may own the grant: it has the same (owner, node) lifetime.
        let ob = OwnerReference { kind: "OwnerBinding".into(), name: "r1-alice".into(), ..Default::default() };
        let owned = api_secret_binding("ws-alice", "alice", "rustic-git-api", "kube-system", Some(&ob));
        assert_eq!(owned.metadata.owner_references.unwrap()[0].kind, "OwnerBinding");
    }

    /// Three things have to line up for git in a workspace to authenticate, and each fails
    /// silently on its own: the mount, the 0400 mode ssh insists on, and the env var that tells
    /// git which key to use.
    #[test]
    fn a_workspace_pod_carries_the_owners_platform_key() {
        let spec = workspace_pod(&ws_spec(), "ws-1", &ctx(), None).spec.unwrap();
        let v = spec.volumes.unwrap().into_iter().find(|v| v.name == "user-key").expect("volume");
        let sv = v.secret.unwrap();
        assert_eq!(sv.secret_name.as_deref(), Some(USER_KEY_SECRET));
        assert_eq!(sv.default_mode, Some(0o444), "git runs as kl and the file is root's");
        // The API writes it after the controller makes the namespace, so it can be late.
        assert_eq!(sv.optional, Some(true));
        let c = &spec.containers[0];
        assert!(c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == "user-key" && m.mount_path == USER_KEY_PATH));
        let env = c.env.as_ref().unwrap().iter().find(|e| e.name == "GIT_SSH_COMMAND").unwrap();
        assert!(env.value.as_ref().unwrap().contains(USER_KEY_PATH));
    }

    /// A private image has to be pullable in the namespace the pod runs in. The kubelet ignores a
    /// named pull secret that does not exist, so referencing it unconditionally costs nothing for a
    /// public image and means a namespace given a credential just works.
    #[test]
    fn tenant_pods_reference_the_namespace_pull_secret() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let refs = p.spec.unwrap().image_pull_secrets.unwrap();
        assert_eq!(refs[0].name, PULL_SECRET);

        let d = service_statefulset(&svc("data", "/data"), "env-1", "team", &ctx()).unwrap();
        let refs = d.spec.unwrap().template.spec.unwrap().image_pull_secrets.unwrap();
        assert_eq!(refs[0].name, PULL_SECRET, "an env's services are where private images show up");
    }

    #[test]
    fn a_namespace_enforces_baseline_and_audits_restricted() {
        let ns = namespace("ws-alice", "alice", "workspace", None);
        let l = ns.metadata.labels.unwrap();
        // baseline blocks hostPath, privileged, hostNetwork/PID/IPC and dangerous capabilities —
        // the actual escape vectors — while leaving root inside the container, which the default
        // image and every common database image need.
        assert_eq!(l.get("pod-security.kubernetes.io/enforce").map(String::as_str), Some("baseline"));
        assert_eq!(l.get("pod-security.kubernetes.io/audit").map(String::as_str), Some("restricted"));
    }

    #[test]
    fn a_workspace_pod_mounts_the_store_and_only_its_own_profile_read_only() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let c = &p.spec.as_ref().unwrap().containers[0];
        let mounts = c.volume_mounts.as_ref().unwrap();
        let store = mounts.iter().find(|m| m.mount_path == "/nix/store").expect("store mount");
        assert_eq!(store.read_only, Some(true));
        assert_eq!(store.sub_path.as_deref(), Some("store"));
        assert_eq!(store.name, "nix");
        let prof = mounts.iter().find(|m| m.mount_path == "/nix/profile").expect("profile mount");
        assert_eq!(prof.read_only, Some(true));
        assert_eq!(prof.sub_path.as_deref(), Some("var/rustic/profiles/ws-1"));
        assert!(!mounts.iter().any(|m| m.mount_path == "/nix"), "never the whole store tree: other profiles and the daemon socket live there");
        let env = c.env.as_ref().unwrap();
        let get = |k: &str| env.iter().find(|e| e.name == k).and_then(|e| e.value.clone()).unwrap();
        // The MOUNT is the directory; every env points at the `current` link inside it, because a
        // subPath is resolved once at container start and a swapped link under it never lands.
        assert!(get("PATH").starts_with("/nix/profile/current/bin:"));
        assert_eq!(get("NIX_PROFILE"), "/nix/profile/current");
        assert_eq!(get("MANPATH"), "/nix/profile/current/share/man:");
        assert!(get("XDG_DATA_DIRS").starts_with("/nix/profile/current/share:"));
        let vols = p.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        let nix = vols.iter().find(|v| v.name == "nix").unwrap();
        assert_eq!(nix.host_path.as_ref().unwrap().path, NIX_ROOT);
        assert!(vols.iter().all(|v| v.persistent_volume_claim.is_none()), "workspace pod must mount a hostPath, not a claim");
    }

    #[test]
    fn the_nix_pv_is_read_only_and_pinned_to_the_node() {
        let pv = local_pv(&nix_pv_name("ws-1"), "ws-acme", &nix_claim_name("ws-1"), NIX_ROOT, "ReadOnlyMany", 1, "acme", &ctx());
        let spec = pv.spec.unwrap();
        assert_eq!(spec.local.as_ref().unwrap().path, "/nix");
        assert_eq!(spec.access_modes.as_deref(), Some(&["ReadOnlyMany".to_string()][..]));
        assert_eq!(spec.persistent_volume_reclaim_policy.as_deref(), Some("Retain"));
        // Every nix PV points at the same /nix, so the pairing has to be exact — it is now the
        // PV that names its one claim.
        let cr = spec.claim_ref.clone().unwrap();
        assert_eq!((cr.namespace.as_deref(), cr.name.as_deref()), (Some("ws-acme"), Some("nix-ws-1")));
        let term = &spec.node_affinity.unwrap().required.unwrap().node_selector_terms[0];
        assert_eq!(term.match_expressions.as_ref().unwrap()[0].values.as_ref().unwrap()[0], ctx().node_name);
        let c = claim("ws-acme", &nix_claim_name("ws-1"), "ReadOnlyMany", 1, "acme", &owner_ref());
        let cs = c.spec.unwrap();
        assert!(cs.volume_name.is_none());
        assert_eq!(cs.access_modes.as_deref(), Some(&["ReadOnlyMany".to_string()][..]));
    }

    #[test]
    fn a_workspace_pod_mounts_its_volume_at_workspace_and_only_there() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let s = p.spec.unwrap();
        let claims = s.volumes.as_ref().unwrap().iter().filter(|v| v.name == "live" && v.host_path.is_some());
        assert_eq!(claims.count(), 1);
        let mounts = s.containers[0].volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.iter().filter(|m| m.name == "live").count(), 1, "the nginx web-root mount is gone with nginx");
        assert!(mounts.iter().any(|m| m.mount_path == "/home/kl/workspaces/dev" && m.read_only.is_none()));
    }

    /// The home is a PV mounted at `/home/kl` and the workspace subvolume a PV mounted INSIDE it;
    /// the kubelet orders mounts by path depth, so the paths carry the order. The ssh Secret
    /// mounts under `/home/kl/.ssh` land inside the home too — a Secret inside a PV is fine.
    #[test]
    fn a_workspace_pod_mounts_the_home_and_the_workspace_inside_it() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let s = p.spec.unwrap();
        let home = s.volumes.as_ref().unwrap().iter().find(|v| v.name == "home").expect("home volume");
        assert_eq!(home.host_path.as_ref().unwrap().path, live_path(ctx().pool, &crate::crd::home_volume_name(&ws_spec().owner)));
        let mounts = s.containers[0].volume_mounts.as_ref().unwrap();
        let home_mount = mounts.iter().find(|m| m.name == "home").expect("home mount");
        assert_eq!(home_mount.mount_path, HOME_DIR);
        assert!(home_mount.read_only.is_none(), "dotfiles are written by the person");
        assert!(home_mount.sub_path.is_none());
        let live = mounts.iter().find(|m| m.name == "live").unwrap();
        assert!(live.mount_path.starts_with(&format!("{HOME_DIR}/")), "the workspace is INSIDE the home: {}", live.mount_path);
        assert!(SSH_HOME.starts_with(HOME_DIR));
        // A custom image gets the home too: it is the person's, not the image's.
        let mut custom = ws_spec();
        custom.image = "ghcr.io/someone/theirs:1".into();
        let s = workspace_pod(&custom, "ws-1", &ctx(), None).spec.unwrap();
        assert!(s.volumes.as_ref().unwrap().iter().any(|v| v.name == "home"));
    }

    /// Four things have to line up for `ssh kl@workspace` to work, and each fails silently on
    /// its own: sshd as the container's process, its host key, the owner's authorized_keys where
    /// the config says to look, and the modes sshd refuses to start (or to authenticate) without.
    #[test]
    fn the_default_image_runs_sshd_with_its_own_host_key_and_the_owners_keys() {
        let mut spec = ws_spec();
        spec.image = crate::model::DEFAULT_WS_IMAGE.into();
        let s = workspace_pod(&spec, "ws-1", &ctx(), None).spec.unwrap();
        let c = &s.containers[0];
        let cmd = c.command.as_ref().unwrap();
        assert_eq!(cmd[0], "/bin/sh");
        assert!(
            cmd[2].trim_end().ends_with(&format!("exec {}/bin/sshd -D -e -f {SSHD_DIR}/sshd_config", crate::packages::PROFILE_LINK)),
            "{}",
            cmd[2]
        );
        // sshd exits on a missing privsep directory or a missing `sshd` user, and stock alpine has
        // neither.
        // The accounts and chroot dir are the image's (Dockerfile `workspace`), not the prelude's.
        assert!(!cmd[2].contains("adduser"), "{}", cmd[2]);
        assert_eq!(c.image.as_deref(), Some("ghcr.io/kloudlite/rustic-git-workspace:deadbeef"), "the pinned image, not the marker");
        assert_eq!(c.ports.as_ref().unwrap()[0].container_port, 22);

        let vols = s.volumes.as_ref().unwrap();
        let host = vols.iter().find(|v| v.name == "ws-ssh").expect("host key volume").secret.clone().unwrap();
        assert_eq!(host.secret_name.as_deref(), Some("ws-ssh-ws-1"));
        // sshd refuses a host key that is group- or world-readable and exits; the config it reads
        // is not a secret and stays readable.
        assert_eq!(host.default_mode, Some(0o400));
        let config_item = host.items.as_ref().unwrap().iter().find(|i| i.key == "sshd_config").expect("config item");
        assert_eq!(config_item.mode, Some(0o444));

        let keys = vols.iter().find(|v| v.name == "authorized-keys").expect("authorized_keys volume").secret.clone().unwrap();
        assert_eq!(keys.secret_name.as_deref(), Some(USER_KEY_SECRET), "the same Secret the API already rewrites");
        let items = keys.items.as_ref().unwrap();
        assert_eq!(items.len(), 1, "only the public half: the private git key must not land in /home/kl/.ssh");
        assert_eq!(items[0].key, "authorized_keys");
        // Root's file (the kubelet writes it), read by sshd AS kl: 0600 would be "Permission
        // denied" on every login. StrictModes is off, so sshd does not mind the width.
        assert_eq!(items[0].mode, Some(0o444));
        assert_eq!(keys.optional, Some(true), "an owner who has registered no key still gets a pod");

        let mounts = c.volume_mounts.as_ref().unwrap();
        let ssh = mounts.iter().find(|m| m.name == "ws-ssh").unwrap();
        assert_eq!(ssh.mount_path, SSHD_DIR);
        assert_eq!(ssh.read_only, Some(true));
        let ak = mounts.iter().find(|m| m.name == "authorized-keys").unwrap();
        // The DIRECTORY, not a subPath of the file: a subPath of an optional Secret wedges the pod
        // in ContainerCreating, and never picks up a key added later.
        assert_eq!(ak.mount_path, SSH_HOME);
        assert_eq!(ak.sub_path, None);
        assert_eq!(ak.read_only, Some(true));
        // Where sshd is told to look has to be where the mount actually puts it.
        assert!(sshd_config("dev").contains(&format!("AuthorizedKeysFile {SSH_HOME}/authorized_keys")));
        // The Secret mount's tmpfs is 1777; without this every registered key is refused.
        assert!(sshd_config("dev").contains("StrictModes no\n"));
        // The account sshd lets in: fixed uid, unlocked, owning the volume; and the key it reads.
        let prelude = &cmd[2];
        // `-h`: the tree is the person's between starts, and a planted symlink must not hand root's
        // chown a target outside it (the same hole the home seed closed by running as kl).
        assert!(prelude.contains("chown -Rh 1000:1000 /home/kl/workspaces/dev"), "{prelude}");
        assert!(!prelude.contains("chown -R 1000:1000"), "{prelude}");
        // Never `-R` over the home: `.ssh` is a read-only mount, and under `set -e` one EROFS
        // from chown is a pod that never starts.
        assert!(!prelude.contains("-R 1000:1000 $H"), "{prelude}");
        // Root's part ends where `su` begins. Below `$H` the person owns the tree between starts,
        // so a root `chown`/`mkdir`/redirect there follows whatever symlink they planted — `$H`
        // itself is a mountpoint and is the one path root may touch.
        let su_at = prelude.lines().position(|l| l.starts_with("su kl -s /bin/sh <<'SEED'")).expect("seed runs as kl");
        let root: Vec<&str> = prelude.lines().take(su_at).collect();
        assert!(root.contains(&"chown 1000:1000 $H $H/workspaces"), "{root:?}");
        for l in &root {
            // Root writes only to /etc (the container's own filesystem); nothing under $H.
            assert!(!l.contains("$H/") || l.starts_with("chown 1000:1000 $H"), "root must not write under $H: {l}");
            assert!(!l.contains("> /home"), "root must not write under the home: {l}");
            assert!(!l.starts_with("chown") || *l == "chown 1000:1000 $H $H/workspaces", "root chown below the mountpoints: {l}");
        }
        let seed_end = prelude.lines().position(|l| l == "SEED").expect("heredoc terminator at column 0");
        assert!(prelude.lines().skip(su_at + 1).take(seed_end - su_at - 1).any(|l| l.starts_with("mkdir -p $H/")), "{prelude}");
        // `~/workspaces` is the pod's own emptyDir: root chowns that mount point and nothing else.
        assert!(prelude.contains("chown 1000:1000 $H $H/workspaces\n"), "{prelude}");
        assert!(prelude.lines().nth(seed_end + 1).unwrap().starts_with("chown -Rh 1000:1000 /home/kl/workspaces/"), "{prelude}");
        // The prompt and the profile's PATH, for both shells; the greeting replaces alpine's.
        assert!(prelude.contains("starship init zsh"), "{prelude}");
        // Coloured `ls` in both shells: coreutils' ls is plain until LS_COLORS and --color say
        // otherwise, and a login that cannot tell a directory from a file feels broken.
        assert!(prelude.contains("dircolors -b"), "{prelude}");
        assert!(prelude.contains("ls --color=auto"), "{prelude}");
        // Seeded once: a person's own edits to their rc files must survive a restart.
        assert!(prelude.contains("[ -e $H/.config/zsh/.zshrc ] ||"), "{prelude}");
        // Run the rc-seeding lines for real: the quoting inside printf is the thing that breaks.
        let home = tempfile::tempdir().unwrap();
        let seed: String = prelude
            .lines()
            .filter(|l| l.contains("printf") || l.starts_with("H=") || l.starts_with("mkdir -p $H"))
            .map(|l| l.replacen("H=/home/kl", &format!("H={}", home.path().display()), 1))
            .collect::<Vec<_>>()
            .join("\n");
        let ok = std::process::Command::new("sh").arg("-c").arg(&seed).status().map(|s| s.success());
        assert_eq!(ok.ok(), Some(true), "seed lines do not run:\n{seed}");
        let zshrc = std::fs::read_to_string(home.path().join(".config/zsh/.zshrc")).unwrap();
        assert!(zshrc.contains("eval \"$(dircolors -b)\"\n") && zshrc.contains("alias ls=\"ls --color=auto\""), "{zshrc}");
        assert!(zshrc.contains("zstyle \":completion:*\" list-colors \"${(s.:.)LS_COLORS}\""), "{zshrc}");
        let fish = std::fs::read_to_string(home.path().join(".config/fish/config.fish")).unwrap();
        assert!(fish.contains("set -gx LS_COLORS (dircolors -b | string match -r \"LS_COLORS=.([^']*)\")[2]\n"), "{fish}");
        assert!(fish.contains("starship init fish | source\n"), "{fish}");
        assert!(prelude.contains("starship init fish | source"), "{prelude}");
        // It is a shell script assembled from string pieces; the one check that catches a broken
        // heredoc or an unbalanced quote before a pod does.
        let ok = std::process::Command::new("sh").arg("-n").arg("-c").arg(prelude).status().map(|s| s.success());
        assert_eq!(ok.ok(), Some(true), "prelude does not parse:\n{prelude}");
        // Non-interactive logins (`ssh ws cmd`, sftp, editors' remote helpers) read no rc file,
        // so the profile's PATH has to come from sshd itself.
        let cfg = sshd_config("dev");
        // Exactly one SetEnv line, carrying every variable: sshd ignores a second one.
        assert_eq!(cfg.matches("SetEnv ").count(), 1, "{cfg}");
        let line = cfg.lines().find(|l| l.starts_with("SetEnv ")).unwrap();
        assert!(line.contains("\"PATH=/nix/profile/current/bin:"), "{line}");
        assert!(line.contains("\"KL_WORKSPACE=/home/kl/workspaces/dev\"") && line.contains("\"KL_WORKSPACE_NAME=dev\""), "{line}");
        // The platform rc files: interactive-only cd into the workspace, starship names it.
        assert!(prelude.contains("> /etc/zshrc") && prelude.contains("> /etc/fish/conf.d/kl.fish") && prelude.contains("> /etc/starship.toml"), "{prelude}");
        assert!(prelude.contains("[[ -o interactive ]] || return 0"), "{prelude}");
        // zsh finds its rc under `~/.config` only if the LOGIN is told so; the entrypoint's env
        // does not reach an ssh session.
        assert!(line.contains("\"ZDOTDIR=/home/kl/.config/zsh\""), "{line}");
        assert!(line.contains("\"GIT_SSH_COMMAND=ssh -i /etc/rustic-git/ssh/id_ed25519 "), "{line}");
        assert!(line.contains("\"GIT_CONFIG_SYSTEM=/etc/rustic-git/ssh/gitconfig\""), "{line}");
        // ...and the pod entrypoint sees the identical list.
        let names: Vec<&str> = c.env.as_ref().unwrap().iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"GIT_CONFIG_SYSTEM") && names.contains(&"PATH") && names.contains(&"GIT_SSH_COMMAND"), "{names:?}");
        assert_eq!(s.hostname.as_deref(), Some("ws"));
        // No fsGroup: it would re-mode the host key Secret too, and sshd refuses a host key
        // anyone but its owner can read.
        assert!(s.security_context.as_ref().and_then(|s| s.fs_group).is_none());
        // The existing git mount must stay where GIT_SSH_COMMAND points.
        assert!(mounts.iter().any(|m| m.name == "user-key" && m.mount_path == USER_KEY_PATH));
        // Storage is mounted from the node, not claimed — nothing here may grow a PVC.
        assert!(vols.iter().all(|v| v.persistent_volume_claim.is_none()));
    }

    #[test]
    fn a_custom_image_keeps_its_entrypoint_and_gets_no_sshd() {
        let mut spec = ws_spec();
        spec.image = "ghcr.io/acme/dev:1".into();
        let s = workspace_pod(&spec, "ws-1", &ctx(), None).spec.unwrap();
        assert!(s.containers[0].command.is_none(), "a user image keeps its entrypoint");
        assert!(s.containers[0].ports.is_none());
        assert!(s.volumes.as_ref().unwrap().iter().all(|v| v.name != "ws-ssh" && v.name != "authorized-keys"));
    }

    /// The host key Secret is per workspace and dies with it — a clone gets its own.
    #[test]
    fn a_workspaces_host_key_lives_and_dies_with_it() {
        let s = ws_ssh_secret("ws-1", "dev", "ws-alice", "alice", &owner_ref(), "PRIVATE", "ssh-ed25519 AAAA ws");
        assert_eq!(s.metadata.name.as_deref(), Some("ws-ssh-ws-1"));
        assert_eq!(s.metadata.namespace.as_deref(), Some("ws-alice"));
        assert_eq!(s.metadata.owner_references.unwrap()[0].controller, Some(true));
        let d = s.string_data.unwrap();
        assert_eq!(d["ssh_host_ed25519_key"], "PRIVATE");
        assert_eq!(d["ssh_host_ed25519_key.pub"], "ssh-ed25519 AAAA ws");
        // The config names the key and the keys file by absolute path, and turns passwords off:
        // the container runs as root, and the login is `kl` — never root.
        let cfg = &d["sshd_config"];
        assert!(cfg.contains(&format!("HostKey {SSHD_DIR}/ssh_host_ed25519_key")), "{cfg}");
        assert!(cfg.contains("AuthorizedKeysFile /home/kl/.ssh/authorized_keys"), "{cfg}");
        assert!(cfg.contains("PermitRootLogin no\n"), "{cfg}");
        assert!(cfg.contains("AllowUsers kl\n"), "{cfg}");
        assert!(cfg.contains("PasswordAuthentication no"), "{cfg}");
    }

    /// Port 22 is open to exactly one peer. Without the namespace half every tenant's own pods
    /// could label themselves `app=rustic-git-gateway` and reach each other's sshd.
    #[test]
    fn only_the_gateway_may_reach_port_22() {
        let p = allow_gateway_ingress("ws-alice", "alice", &owner_ref());
        assert_eq!(p.metadata.name.as_deref(), Some("allow-gateway-ssh"));
        let spec = p.spec.unwrap();
        assert_eq!(spec.policy_types.as_ref().unwrap(), &vec!["Ingress".to_string()], "never an egress hole");
        let rule = &spec.ingress.as_ref().unwrap()[0];
        assert_eq!(rule.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(22)));
        let from = rule.from.as_ref().unwrap();
        assert_eq!(from.len(), 1, "one peer: namespace AND pod, not namespace OR pod");
        let ns = from[0].namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
        assert_eq!(ns["kubernetes.io/metadata.name"], GATEWAY_NAMESPACE);
        assert_eq!(GATEWAY_NAMESPACE, "rustic-git-system", "deploy/k3s/gateway.yaml puts the gateway here; keep them equal");
        let pod = from[0].pod_selector.as_ref().unwrap().match_labels.as_ref().unwrap();
        assert_eq!(pod["app"], "rustic-git-gateway");
    }

    /// The grant selects the POD, never the namespace: an owner's workspaces share a namespace, so
    /// a namespace-wide rule would open every workspace they have to the environment.
    #[test]
    fn the_attachment_egress_selects_one_workspace_pod() {
        let p = attach_egress("ws-acme", "ws-1", "env-abc", "acme", &owner_ref());
        assert_eq!(p.metadata.name.as_deref(), Some("attach-ws-1"));
        assert_eq!(p.metadata.namespace.as_deref(), Some("ws-acme"));
        let spec = serde_json::to_value(p.spec.unwrap()).unwrap();
        assert_eq!(spec["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
        assert_eq!(spec["policyTypes"], serde_json::json!(["Egress"]));
        assert_eq!(
            spec["egress"][0]["to"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "env-abc"
        );
    }

    /// The environment side names both the namespace and the pod: a namespace selector alone would
    /// admit every workspace of every owner who happens to share that namespace.
    #[test]
    fn the_attachment_ingress_names_the_namespace_and_the_pod() {
        let p = attach_ingress("env-abc", "ws-acme", "ws-1", "acme", &owner_ref());
        assert_eq!(p.metadata.namespace.as_deref(), Some("env-abc"));
        let spec = serde_json::to_value(p.spec.unwrap()).unwrap();
        let from = &spec["ingress"][0]["from"][0];
        assert_eq!(from["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"], "ws-acme");
        assert_eq!(from["podSelector"]["matchLabels"][WORKSPACE_LABEL], "ws-1");
        assert_eq!(spec["policyTypes"], serde_json::json!(["Ingress"]));
    }

    #[test]
    fn every_child_object_cascades_on_delete() {
        // Reclamation via garbage collection rather than cleanup code that can be skipped or crash
        // halfway. If this regresses, deleting a workspace leaks its pod, namespace and PV.
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        assert_eq!(p.metadata.owner_references.unwrap()[0].controller, Some(true));
        let pv = local_pv(&pv_name("ws-1"), "ws-alice", &claim_name("ws-1"), &live_path(ctx().pool, "ws-1"), "ReadWriteOnce", 20, "alice", &ctx());
        assert_eq!(pv.metadata.owner_references.unwrap().len(), 1);
        assert_eq!(namespace("env-1", "team", "environment", Some(&owner_ref())).metadata.owner_references.unwrap().len(), 1);
        for pol in default_policies("env-1", "team", &owner_ref()) {
            assert_eq!(pol.metadata.owner_references.unwrap().len(), 1);
        }

        // The shared user namespace must NOT cascade: it outlives any one workspace, and an owner
        // reference here would delete every sibling when one workspace goes.
        let shared = namespace("ws-alice", "alice", "workspace", None);
        assert!(
            shared.metadata.owner_references.is_none(),
            "a user's workspace namespace is shared infrastructure and must not be garbage-collected"
        );
    }

    #[test]
    fn an_environment_namespace_denies_by_default_and_still_resolves_dns() {
        let pols = default_policies("env-1", "team", &owner_ref());
        let names: Vec<_> = pols.iter().filter_map(|p| p.metadata.name.as_deref()).collect();
        assert_eq!(names, vec!["default-deny", "allow-dns", "allow-internet-egress", "allow-same-namespace"]);

        let deny = pols[0].spec.as_ref().unwrap();
        assert_eq!(deny.policy_types.as_ref().unwrap().len(), 2, "deny must cover BOTH directions");
        assert!(deny.ingress.is_none() && deny.egress.is_none(), "a rule here would stop it denying");

        let dns = pols[1].spec.as_ref().unwrap().egress.as_ref().unwrap();
        assert!(dns[0].ports.as_ref().unwrap().iter().any(|p| p.port == Some(IntOrString::Int(53))));
    }

    /// A workspace has to reach npm and GitHub, but "allow egress" written the obvious way
    /// (`0.0.0.0/0`) also opens `169.254.169.254` — the cloud metadata service, which on Azure
    /// hands out the NODE's managed identity. That is an escape from the cluster, not the
    /// namespace, so the internet rule must be an allow-list with holes punched out.
    #[test]
    fn internet_egress_never_reaches_the_metadata_service_or_the_cluster() {
        let pols = default_policies("ws-alice", "alice", &owner_ref());
        let net = pols.iter().find(|p| p.metadata.name.as_deref() == Some("allow-internet-egress")).unwrap();
        let rules = net.spec.as_ref().unwrap().egress.as_ref().unwrap();
        let block = rules[0].to.as_ref().unwrap()[0].ip_block.as_ref().unwrap();
        assert_eq!(block.cidr, "0.0.0.0/0");
        let except = block.except.as_ref().unwrap();

        // The metadata service, and every private range the cluster lives on.
        for cidr in ["169.254.0.0/16", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
            assert!(except.contains(&cidr.to_string()), "{cidr} must be excluded from egress");
        }
        // Egress-only: this rule must never become an ingress hole.
        assert_eq!(net.spec.as_ref().unwrap().policy_types.as_ref().unwrap(), &vec!["Egress".to_string()]);
    }

    #[test]
    fn a_service_gets_a_clusterip_for_each_declared_port() {
        let s = service_clusterip(&svc("data", "/data"), "env-1", "team", &owner_ref());
        let spec = s.spec.unwrap();
        let ports = spec.ports.unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 80);
        assert_eq!(ports[0].target_port, Some(IntOrString::Int(80)));
        // The selector must match the Deployment's template labels or the Service selects nothing
        // and the name resolves to a black hole.
        assert_eq!(spec.selector.unwrap().get(SERVICE_LABEL).map(String::as_str), Some("web"));
    }
}

//! The per-workspace Secrets: the owner's platform key, git identity and registry token
//! (`user-key`), the pull credential, and the sshd host key with the sshd_config that names them.
//! Nothing here is long-lived — every value is re-projected on the keys beat.

use super::*;


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
pub const USER_KEY_PATH: &str = "/etc/kloudlite/ssh";


/// The owner's private key as a namespace Secret. Written by the API tier, which holds `secrets`
/// only in namespaces the controller has vouched for — see `api_secret_binding`.
pub fn user_key_secret(
    owner: &str,
    namespace: &str,
    private_openssh: &str,
    m: &crate::api::OwnerMaterial,
    authorized_keys: &str,
    registry_token: &str,
) -> Secret {
    Secret {
        // No ownerReference: the key belongs to the OWNER, not to any one workspace, so deleting
        // the workspace that happened to trigger its creation must not take it with them.
        metadata: ObjectMeta {
            name: Some(USER_KEY_SECRET.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels(owner, "workspace")),
            ..Default::default()
        },
        // The public keys sshd admits are a CLUSTER fact now, projected per owner namespace as
        // `OwnerKeys` and rendered to disk by every node's agent, so a key added in the UI reaches
        // a running pod without a Secret rewrite per namespace. The entry below is TRANSITIONAL:
        // pods of an agent that has not been upgraded yet still mount it, and it goes away in the
        // release after every region's agent is on this build (spec §5 step 4).
        string_data: Some(BTreeMap::from([
            ("id_ed25519".to_string(), private_openssh.to_string()),
            ("authorized_keys".to_string(), authorized_keys.to_string()),
            // Read by git as its SYSTEM config (`GIT_CONFIG_SYSTEM`), so `~/.gitconfig` still
            // overrides it and a changed display name reaches running workspaces with the next
            // Secret rewrite, no restart. git's own escaping: a name with a quote is quoted.
            ("gitconfig".to_string(), gitconfig(&m.git_name, &m.git_email)),
            // 24h, re-minted every `KEYS_RESYNC_SECS` beat by whoever calls this — rotation is
            // just the next beat, no revocation code needed. `"*"` because authorization is
            // re-checked per registry request against the image, never trusted from the scope.
            ("registry-token".to_string(), registry_token.to_string()),
        ])),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}


pub(super) fn gitconfig(name: &str, email: &str) -> String {
    let q = |v: &str| v.replace('\\', "\\\\").replace('"', "\\\"");
    format!("[user]\n\tname = \"{}\"\n\temail = \"{}\"\n", q(name), q(email))
}


/// The per-workspace host key Secret's name.
pub fn ws_ssh_secret_name(id: &str) -> String {
    format!("ws-ssh-{id}")
}


/// sshd's whole configuration, generated so `sshd_config` and the mounts that satisfy it cannot
/// drift apart.
///
/// `PermitRootLogin no` and `AllowUsers kl`: the only way in is a key the owner registered, and
/// it opens a shell as `kl`, never as the root the container itself runs as. `StrictModes no` because the file arrives as a
/// hostPath mount whose parent directories are the node's, not `kl`'s — sshd would refuse every
/// key in it as "bad ownership or modes" otherwise. The mode it would have checked is enforced
/// where the file is written instead (`agent::controller::keys`: 0600, owned by `kl`).
/// `ClientAliveInterval 30` is not a nicety — Cloudflare idles a
/// WebSocket after 100s, and the tunnel is the whole data path.
pub fn sshd_config(name: &str, owner: &str, registry_host: &str) -> String {
    let set_env = format!("SetEnv {}", login_env(name, owner, registry_host).iter().map(|e| format!("\"{}={}\"", e.name, e.value.as_deref().unwrap_or_default())).collect::<Vec<_>>().join(" "));
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


/// This workspace's ed25519 host key, generated once by the node that owns it.
///
/// Per workspace and owned BY the workspace: it is the identity users pin in `known_hosts`, so it
/// must survive pod recreation (hence a Secret, not a file on the subvolume) and die with the
/// workspace (hence the ownerReference — a clone is a different host and gets its own).
#[allow(clippy::too_many_arguments)]
pub fn ws_ssh_secret(
    id: &str,
    name: &str,
    namespace: &str,
    owner: &str,
    owner_ref: &OwnerReference,
    private_openssh: &str,
    public_line: &str,
    registry_host: &str,
) -> Secret {
    Secret {
        metadata: meta(&ws_ssh_secret_name(id), Some(namespace), owner, "workspace", owner_ref),
        string_data: Some(BTreeMap::from([
            ("ssh_host_ed25519_key".to_string(), private_openssh.to_string()),
            ("ssh_host_ed25519_key.pub".to_string(), public_line.to_string()),
            ("sshd_config".to_string(), sshd_config(name, owner, registry_host)),
        ])),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

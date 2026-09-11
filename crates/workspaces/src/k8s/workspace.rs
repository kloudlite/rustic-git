//! The workspace Pod: the login environment, the hardened security context, every hostPath
//! volume (home on NFS, caches local, the worktree, the projected keys), node placement, the git
//! seed init container, and `workspace_pod` that composes them.

use super::*;


/// The environment a workspace shell sees, whether it is the image's entrypoint or an ssh login:
/// the Nix profile on PATH, git's key and identity. ONE list, because sshd does not inherit the
/// container's environment and two lists would drift.
/// The remote buildkit gate every workspace pod builds through — never a daemon in the pod
/// itself, which would need privilege the sandbox exists to remove.
pub const BUILDKIT_HOST: &str = "tcp://builder-gate.kloudlite-system.svc:1234";


pub(super) fn login_env(name: &str, owner: &str, registry_host: &str) -> Vec<EnvVar> {
    let var = |n: &str, v: String| EnvVar { name: n.into(), value: Some(v), ..Default::default() };
    vec![
        git_ssh_command(),
        // Which workspace this shell is in: the platform rc files cd into it and the prompt
        // names it. Per pod, which is why sshd's SetEnv is generated per workspace.
        var("KL_WORKSPACE", workspace_dir(name)),
        var("KL_WORKSPACE_NAME", name.to_string()),
        var("GIT_CONFIG_SYSTEM", format!("{USER_KEY_PATH}/gitconfig")),
        var("BUILDKIT_HOST", BUILDKIT_HOST.to_string()),
        var("KL_OWNER", owner.to_string()),
        var("KL_REGISTRY_HOST", registry_host.to_string()),
        // ponytail: an image with a non-standard PATH loses it; read it from the image config
        // via the registry if that ever matters.
        var("PATH", crate::packages::path_env(None)),
        var("NIX_PROFILE", crate::packages::PROFILE_LINK.into()),
        // zsh's rc lives under `~/.config` with fish's, so the persistent home carries every shell's
        // config in one tree; without this zsh reads `~/.zshrc` and finds nothing.
        var("ZDOTDIR", format!("{HOME_DIR}/.config/zsh")),
        // No locale at all leaves zsh with MULTIBYTE off: starship's `❯` then counts as three
        // columns, every completion redraw lands two columns right of the word, and the person
        // sees "cacargo" for a buffer that reads "cargo". musl ships C.UTF-8 with no locale files
        // to install, so it is the one value every image can honour.
        var("LANG", "C.UTF-8".into()),
        var("MANPATH", format!("{}/share/man:", crate::packages::PROFILE_LINK)),
        var("XDG_DATA_DIRS", format!("{}/share:/usr/local/share:/usr/share", crate::packages::PROFILE_LINK)),
        // Three homes, one rule each. The NFS home keeps small config. The WORKSPACE DIR keeps
        // the project and EVERY cache — build output, package stores, toolchains, editor servers
        // — under `{ws}/.cache/`, so a clone, a restore or a start on another node arrives warm:
        // it is snapshotted and replicated with the tree, where a per-node cache was rebuilt from
        // nothing on every move (the owner's call, 2026-09-11: "everything that can be cached
        // goes in the workspace folder"). `homecache`, node-local, keeps only what must not
        // travel: `TMPDIR` and shell state.
        //
        // Under `{ws}/.cache/`, never a tool's own `./target`: nothing the platform places may
        // collide with a directory a repository versions, and the global git ignore
        // (`/etc/kloudlite/gitignore-global`, appended to `~/.config/git/ignore` by `prelude`) is
        // one line per kind rather than one per tool.
        var("CARGO_TARGET_DIR", format!("{}/.cache/cargo-target", workspace_dir(name))),
        var("GOCACHE", format!("{}/.cache/go-build", workspace_dir(name))),
        var("PLAYWRIGHT_BROWSERS_PATH", format!("{}/.cache/ms-playwright", workspace_dir(name))),
        var("XDG_CACHE_HOME", format!("{}/.cache/xdg", workspace_dir(name))),
        var("npm_config_cache", format!("{}/.cache/npm", workspace_dir(name))),
        var("PNPM_STORE_DIR", format!("{}/.cache/pnpm", workspace_dir(name))),
        var("BUN_INSTALL_CACHE_DIR", format!("{}/.cache/bun", workspace_dir(name))),
        // NOT CARGO_HOME: it holds `credentials.toml` and `config.toml` — configs, which is the
        // half of the home that must survive. Cargo has no separate knob for its registry cache,
        // so that part is a MOUNT of `{ws}/.cache/cargo-registry` at `~/.cargo/registry` instead
        // (see `workspace_pod`).
        var("RUSTUP_HOME", format!("{}/.cache/rustup", workspace_dir(name))),
        // GOMODCACHE only, never GOPATH: GOPATH also holds `src/` and `bin/`, which are the
        // person's own files, and the module cache is the only large rebuildable part of it.
        var("GOMODCACHE", format!("{}/.cache/gomod", workspace_dir(name))),
        // `GRADLE_USER_HOME` holds `gradle.properties` credentials — config, the home's half,
        // the same shape as `CARGO_HOME`; Gradle's project cache is `{ws}/.gradle` on its own.
        var("GRADLE_USER_HOME", format!("{HOME_DIR}/.gradle")),
        var("MAVEN_OPTS", format!("-Dmaven.repo.local={}/.cache/m2", workspace_dir(name))),
        var("YARN_CACHE_FOLDER", format!("{}/.cache/yarn", workspace_dir(name))),
        var("COMPOSER_CACHE_DIR", format!("{}/.cache/composer", workspace_dir(name))),
        var("NUGET_PACKAGES", format!("{}/.cache/nuget", workspace_dir(name))),
        var("TMPDIR", format!("{HOME_CACHE_DIR}/tmp")),
        var("DO_NOT_TRACK", "1".into()),
        var("UV_CACHE_DIR", format!("{}/.cache/uv", workspace_dir(name))),
        var("PIP_CACHE_DIR", format!("{}/.cache/pip", workspace_dir(name))),
        var("DENO_DIR", format!("{}/.cache/deno", workspace_dir(name))),
        // History is per-node write traffic on every keystroke; keeping it off NFS is why it gets
        // its own var instead of riding HOME_CACHE_DIR — it isn't a cache, it's state worth keeping.
        var("HISTFILE", format!("{HOME_STATE_DIR}/shell_history")),
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
/// Root chowns only mountpoints and the directories the kubelet made to hold them, never a path
/// the person could have replaced: `$H`, `$H/workspaces`, and `~/.cargo` with `~/.cargo/registry`.
/// The last two are the exception and exist because the kubelet creates the missing PARENT of a
/// subPath mount as root:root 0755 — leaving `CARGO_HOME` on the shared home (which is the point:
/// `credentials.toml` and `config.toml` must survive) but unwritable, and the mount point itself
/// root-owned so cargo cannot fill the cache either. `.vscode-server`/`.cursor-server` need no
/// such fix: they are leaf mount points with no root-owned parent, and the editors recreate them.
/// A mountpoint cannot be a symlink, and `-h` covers the parent in case the kubelet followed a
/// planted one rather than creating it. Everything else below `$H` — the mkdirs, the rc seeds — runs as `kl` via `su`,
/// because the home is now persistent and the person owns every byte of it between starts:
/// `mv ~/.config x; ln -s /etc ~/.config` would otherwise make the next start `chown` and write
/// through `/etc` as root, and the container keeps CHOWN/DAC_OVERRIDE on a writable rootfs. The
/// seed runs from a heredoc on `su`'s stdin (busybox `su -c` would need the printf quoting nested
/// a second time), with `set -e` of its own so a failed seed still stops the pod.
/// ponytail: `chown -R` walks the whole volume on every start; fine for source trees. `$H` is the
/// persistent home hostPath and the rc files are seeded only if absent, so a person's own edits survive
/// a restart and a new workspace alike; `~/workspaces/<id>` is a mount point inside it that the
/// kubelet makes, which is why nothing here mkdirs it.
pub(super) fn prelude(name: &str) -> String {
    let workspace_dir = workspace_dir(name);
    let profile = crate::packages::PROFILE_LINK;
    let path = crate::packages::path_env(None);
    format!(
        "set -e\n\
         H=/home/{SSH_USER}\n\
         chown {SSH_UID}:{SSH_UID} $H $H/workspaces\n\
         chown -h {SSH_UID}:{SSH_UID} $H/.cargo $H/.cargo/registry\n\
         chown {SSH_UID}:{SSH_UID} $H/.local\n\
         mkdir -p /etc/fish/conf.d\n\
         printf '%s\\n' '[[ -o interactive ]] || return 0' '[ \"$PWD\" = \"$HOME\" ] && [ -d \"$KL_WORKSPACE\" ] && cd \"$KL_WORKSPACE\"' '[ -e \"$HOME/.config/starship.toml\" ] || export STARSHIP_CONFIG=/etc/starship.toml' 'mkdir -p \"${{XDG_CACHE_HOME:-$HOME/.cache}}/zsh\"' 'autoload -Uz compinit && compinit -d \"${{XDG_CACHE_HOME:-$HOME/.cache}}/zsh/zcompdump\"' 'zstyle \":completion:*\" menu select' '[ -r /etc/profile.d/kl-build.sh ] && sh /etc/profile.d/kl-build.sh' > /etc/zshrc\n\
         printf '%s\\n' 'status is-interactive; or exit' 'if test \"$PWD\" = \"$HOME\" -a -d \"$KL_WORKSPACE\"; cd \"$KL_WORKSPACE\"; end' 'test -e \"$HOME/.config/starship.toml\"; or set -gx STARSHIP_CONFIG /etc/starship.toml' 'test -r /etc/profile.d/kl-build.sh; and sh /etc/profile.d/kl-build.sh' > /etc/fish/conf.d/kl.fish\n\
         printf '%s\\n' 'format = \"$directory$git_branch$git_status$cmd_duration$line_break$character\"' > /etc/starship.toml\n\
         su {SSH_USER} -s /bin/sh <<'SEED'\n\
         set -e\n\
         export PATH={path}\n\
         H=/home/{SSH_USER}\n\
         mkdir -p $H/.config/fish $H/.config/zsh $H/.config/git $H/.local-cache/tmp\n\
         grep -qF '# kloudlite: derived state' $H/.config/git/ignore 2>/dev/null || cat /etc/kloudlite/gitignore-global >> $H/.config/git/ignore\n\
         [ -e $H/.config/zsh/.zshrc ] || printf 'export PATH={path}\\neval \"$(dircolors -b)\"\\nzstyle \":completion:*\" list-colors \"${{(s.:.)LS_COLORS}}\"\\nalias ls=\"ls --color=auto\" grep=\"grep --color=auto\"\\neval \"$(starship init zsh)\"\\n' > $H/.config/zsh/.zshrc\n\
         [ -e $H/.config/fish/config.fish ] || printf 'set -gx PATH {path}\\nset -gx LS_COLORS (dircolors -b | string match -r \"LS_COLORS=.([^\\047]*)\")[2]\\nalias ls=\"ls --color=auto\"\\nalias grep=\"grep --color=auto\"\\nstarship init fish | source\\n' > $H/.config/fish/config.fish\n\
         SEED\n\
         chown -Rh {SSH_UID}:{SSH_UID} {workspace_dir}\n\
         su {SSH_USER} -s /bin/sh -c 'cd {workspace_dir} && KL_WORKSPACE={workspace_dir} HOME=$H exec kl ide serve >> $H/.local/state/kl-ide.log 2>&1' &\n\
         exec {profile}/bin/sshd -D -e -f {SSHD_DIR}/sshd_config\n"
    )
}


/// `/etc/ssh` for the pod. The private key needs 0400 — sshd exits rather than read a host key
/// anything else can — while the config it reads at the same time is not a secret.
pub(super) fn ws_ssh_volume(id: &str) -> Volume {
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


pub(super) fn user_key_volume(required: bool) -> Volume {
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


/// The `git-seed` init container's OWN view of `user-key`: `id_ed25519` only, via `items`. That
/// container clones over SSH and never reads `gitconfig`/`authorized_keys` — let alone
/// `registry-token`, an api-tier build credential a root init image has no business seeing. The
/// main container keeps the unrestricted `user_key_volume` above; this is a second volume over
/// the same Secret; a Secret is JSON, so serving one key from it costs nothing extra.
pub(super) fn user_key_seed_volume(required: bool) -> Volume {
    Volume {
        name: "user-key-seed".to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(USER_KEY_SECRET.to_string()),
            default_mode: Some(0o444),
            optional: Some(!required),
            items: Some(vec![KeyToPath { key: "id_ed25519".into(), path: "id_ed25519".into(), mode: None }]),
        }),
        ..Default::default()
    }
}


/// The host Nix store, mounted read-only via `hostPath` into every workspace pod.
pub const NIX_ROOT: &str = "/nix";


/// A typed host directory. `Directory` rather than the default: an untyped `hostPath` CREATES a
/// missing path as an empty directory, so a pod that lands where its subvolume is not would start
/// with a blank home and no error. Typed, the kubelet refuses the mount and the pod says so.
///
/// Read-only intent is not expressed here — a `hostPath` volume has no such field — it lives,
/// enforced, on the `VolumeMount`s that reference it.
pub(super) fn host_dir(name: &str, path: String) -> Volume {
    Volume {
        name: name.to_string(),
        host_path: Some(HostPathVolumeSource { path, type_: Some("Directory".into()) }),
        ..Default::default()
    }
}


/// Per-container hardening. The namespace floor is `privileged` now (hostPath mounts require it),
/// so PSA no longer refuses `hostNetwork`/`hostPID`/`hostIPC`, privileged containers or stray
/// hostPath sources — `deploy/k3s/workspace-admission.yaml` is what refuses those instead, as a
/// ValidatingAdmissionPolicy. This security context is the remaining, narrower layer: what the
/// CONTAINER'S OWN runtime surface looks like (capabilities, seccomp, escalation) once the pod has
/// already been admitted.
///
/// `run_as_non_root` is deliberately absent — see the module docs: forcing it would break the
/// zero-configuration default image and most database images an environment is built from.
pub(super) fn hardened() -> SecurityContext {
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
            // This adds back exactly the container runtime's ordinary default, stated explicitly —
            // not a widening of it. But nothing above `privileged` stops us from adding SYS_ADMIN,
            // NET_RAW, SYS_PTRACE or anything else here: the namespace no longer refuses dangerous
            // capabilities — this fixed list is the only thing that does.
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


/// The owner's persistent home: one region-shared NFS export, `{pool}/homes/{owner}`, so every
/// node the owner lands on sees the same dotfiles and history — no per-node btrfs subvolume, no
/// materialize-on-first-landing.
pub(super) fn home_volume(pool: &str, owner: &str) -> Volume {
    host_dir("home", format!("{pool}/homes/{owner}"))
}


/// The owner's node-local cache: editor servers, package-manager caches and shell state that are
/// large, ephemeral and would otherwise cross the network on every read — kept off the shared NFS
/// home and pinned to one node's btrfs, ONE volume with subPaths per use (see the mounts below) so
/// the janitor deletes a single subvolume to reclaim it all.
pub(super) fn homecache_volume(pool: &str, owner: &str) -> Volume {
    host_dir("homecache", format!("{pool}/homecache/{owner}"))
}


/// An emptyDir for `WORKSPACES_DIR`. Per pod on purpose — see the mount's comment.
pub(super) fn workspaces_volume() -> Volume {
    Volume { name: "workspaces".to_string(), empty_dir: Some(Default::default()), ..Default::default() }
}


/// `{pool}/vol/{volume}/live/{ws}` — the per-worktree layout (mirrors
/// `engine::pool::Pool::worktree`, which this crate cannot depend on directly: `engine` is
/// gated behind btrfs tooling this crate's tests don't need).
pub(super) fn worktree_path(pool: &str, volume: &str, ws: &str) -> String {
    format!("{pool}/vol/{volume}/live/{ws}")
}


/// The pod's `live` mount: the WORKTREE path — `volume` and `ws` differ for a shared-volume
/// clone, so both are threaded in rather than reusing the pod's own id.
pub(super) fn live_worktree_volume(pool: &str, volume: &str, ws: &str) -> Volume {
    host_dir("live", worktree_path(pool, volume, ws))
}


/// Keep the pod on a pool node and on the node holding its subvolume, and tolerate the pool
/// taint.
///
/// Two selectors, two jobs: the pool label says the data this pod mounts lives on this node
/// (a node without one cannot host it), the hostname pins the pod to the specific node holding
/// its subvolume. That pin used to come from the PV's `nodeAffinity`; with the volumes mounted from
/// the host there is no PV to carry it, and an unpinned pod would mount an empty directory on the
/// wrong node. The toleration is not optional: the label without it schedules nothing.
pub(super) fn placement(spec: &mut PodSpec, node: &str) {
    spec.node_selector = Some(BTreeMap::from([
        ("kloudlite.io/pool".to_string(), "true".to_string()),
        ("kubernetes.io/hostname".to_string(), node.to_string()),
    ]));
    spec.tolerations = Some(vec![Toleration {
        key: Some("kloudlite.io/pool".to_string()),
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
pub(super) fn git_ssh_command() -> EnvVar {
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
/// if anyone asks for the history they did not ask to clone. NOT `--single-branch`, though: that
/// narrows the fetch refspec to the seeded branch, and a person who then runs `git fetch` never
/// sees `master` appear on a repo whose default is `main` — the branch exists, their clone just
/// stopped asking for it. Every branch tip at depth 1 costs nothing a person would notice.
pub fn git_init_container(
    source: &crate::crd::VolumeSource,
    init_image: &str,
    ssh_host: &str,
    ssh_port: &str,
) -> Result<Option<Container>, String> {
    let crate::crd::VolumeSource::GitRepo { repo, branch } = source else { return Ok(None) };
    let ok = repo.split_once('/').is_some_and(|(o, n)| {
        kloudlite_storage::store::valid_owner(o) && kloudlite_storage::store::valid_segment(n)
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
            // The key volume is 0444 on purpose (`user_key_volume`), which ssh accepts from `kl`
            // and refuses from ROOT — and this container is root. A private copy at 0600 is the
            // one form both callers accept; the root filesystem here is writable, so /tmp exists.
            // Retried IN PLACE, re-reading the key each time, for two minutes: right after a
            // platform-key rotation the kubelet's secret cache still serves the old key for up to
            // a minute, and a crash-looping init container backs off past the window in which
            // the mount refreshes. A clone the server refused is wiped before the next attempt.
            format!(
                "set -e; [ \"$(ls -A {SEED_DIR})\" ] && exit 0; \
                 for i in $(seq 1 24); do install -m 600 {USER_KEY_PATH}/id_ed25519 /tmp/seed_key; \
                 echo \"seed key $(ssh-keygen -lf /tmp/seed_key 2>/dev/null | cut -d\" \" -f2)\"; \
                 echo \"seed key $(ssh-keygen -lf /tmp/seed_key 2>/dev/null | cut -d\" \" -f2)\"; \
                 GIT_SSH_COMMAND=\"ssh -i /tmp/seed_key -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new\" \
                 git clone --depth 1 --no-single-branch --branch \"$BRANCH\" -- \"$URL\" {SEED_DIR} && exit 0; \
                 rm -rf {SEED_DIR}/* {SEED_DIR}/.[!.]* 2>/dev/null || true; sleep 5; done; exit 1"
            ),
        ]),
        env: Some(vec![
            EnvVar { name: "URL".to_string(), value: Some(url), ..Default::default() },
            EnvVar { name: "BRANCH".to_string(), value: Some(branch.clone()), ..Default::default() },
        ]),
        volume_mounts: Some(vec![
            VolumeMount { name: "live".to_string(), mount_path: SEED_DIR.to_string(), ..Default::default() },
            VolumeMount {
                name: "user-key-seed".to_string(),
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


/// The handle whose `OwnerKeys` this pod's namespace sees — the same pair `ws_namespace` is keyed
/// by, since the namespace is what a key set is scoped to: a team's members share one file.
pub fn keys_owner(spec: &WorkspaceSpec) -> &str {
    owner_slug(&spec.owner, &spec.team)
}


/// The same fold on the two strings a handler has before there is a spec — `ensure_builder` names
/// the builder with this and `prune_builders` decides what to keep with `keys_owner`, so they MUST
/// be one function: a workspace with `owner: "alice", team: "Alice"` folded two ways created
/// `bld-Alice` and kept `alice`, and the next beat deleted the builder the create had just made.
///
/// `team == owner` is the personal namespace spelled the long way — `ws_namespace` folds it, and a
/// second file under the same name written by two handles would be one node's race.
pub fn owner_slug<'a>(owner: &'a str, team: &'a str) -> &'a str {
    if team.is_empty() || team.eq_ignore_ascii_case(owner) {
        owner
    } else {
        team
    }
}


/// The workspace's one pod.
/// `ws_id` names the pod and every per-workspace resource on it. `id` is the VOLUME (`volumeRef`)
/// and is used only for the worktree path's root — the two differ for a shared-volume clone
/// (`id` is the source volume; `ws_id` is this workspace's own
/// worktree name) — see `Pool::worktree`.
pub fn workspace_pod(
    spec: &WorkspaceSpec,
    id: &str,
    ws_id: &str,
    ctx: &PodContext,
    init: Option<Container>,
) -> Result<Pod, String> {
    // The last place before `spec.name` becomes a root `/bin/sh -c` word, an sshd `SetEnv` value
    // and this container's `mount_path`. `/v1` checked it; this covers a Workspace written by any
    // other path, exactly as `git_init_container` and `service_statefulset` do for their inputs.
    if !crate::model::valid_ws_name(&spec.name) {
        return Err(format!("workspace name {:?} is not a name", spec.name));
    }
    // ssh is a feature of the DEFAULT image only: a user image brings its own entrypoint, and we
    // cannot replace it with sshd without breaking whatever it was built to run.
    let default_image = crate::model::is_default_image(&spec.image);
    let mut ssh_mounts = vec![];
    if default_image {
        ssh_mounts = vec![
            VolumeMount { name: "ws-ssh".into(), mount_path: SSHD_DIR.into(), read_only: Some(true), ..Default::default() },
            VolumeMount { name: "authorized-keys".into(), mount_path: AUTHORIZED_KEYS_PATH.into(), read_only: Some(true), ..Default::default() },
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
            // Ready means a person can get in, not that the container started. Without a probe
            // the pod's Ready condition flipped the instant the process ran, `/v1` said `ready`,
            // and a command run in that same second failed in a sandbox that could not yet run
            // `su`: the weekly's `homes.cross.node` read the moved home and got exit 1 with
            // nothing printed, where the same read a moment later succeeded. sshd listening is
            // the one signal that covers every way in — ssh, the gateway tunnel, the probe's exec
            // — and only the default image is known to run it, so only it is gated on it.
            readiness_probe: default_image.then(|| Probe {
                tcp_socket: Some(TCPSocketAction { port: IntOrString::Int(22), ..Default::default() }),
                period_seconds: Some(2),
                failure_threshold: Some(3),
                ..Default::default()
            }),
            volume_mounts: Some(vec![
                // Listed before the workspace mount for the reader; the kubelet orders by path
                // depth and `workspace_dir(name)` is under `HOME_DIR`, so the order is implied either way.
                //
                // `HostToContainer` is load-bearing, not hygiene: this binds a path INSIDE the
                // node's shared-home NFS mount, and with the default (`None`) the bind is resolved
                // once at pod start and never again. Replace that mount on the node — a ZeroFS
                // restart, an agent remount, the stale-mount repair in `mount_homes` — and every
                // already-running pod keeps pointing at the detached one, where every access fails
                // "Network is unreachable" until someone recreates the pod. Observed exactly that
                // way. Propagation lets a running pod follow the node's remount instead.
                VolumeMount {
                    name: "home".to_string(),
                    mount_path: HOME_DIR.to_string(),
                    mount_propagation: Some("HostToContainer".to_string()),
                    ..Default::default()
                },
                // One `homecache` volume, five subPaths: the janitor reclaims all of it by
                // deleting a single node-local subvolume, and each subPath is resolved once at
                // container start so `login_env`'s redirected vars actually land here.
                VolumeMount { name: "homecache".to_string(), mount_path: HOME_CACHE_DIR.to_string(), sub_path: Some("cache".to_string()), ..Default::default() },
                // Cargo's registry cache, mounted rather than redirected: `CARGO_HOME` stays on
                // the shared home so `credentials.toml` survives, and cargo offers no env var for
                // the cache alone — so the cache subtree is what moves into the workspace.
                // Every one of these is a subPath of the LIVE subvolume's `.cache/`: the kubelet
                // makes the directory if it is absent, and it travels with the tree.
                VolumeMount { name: "live".to_string(), mount_path: "/home/kl/.cargo/registry".to_string(), sub_path: Some(".cache/cargo-registry".to_string()), ..Default::default() },
                VolumeMount { name: "live".to_string(), mount_path: "/home/kl/.vscode-server".to_string(), sub_path: Some(".cache/vscode-server".to_string()), ..Default::default() },
                VolumeMount { name: "live".to_string(), mount_path: "/home/kl/.cursor-server".to_string(), sub_path: Some(".cache/cursor-server".to_string()), ..Default::default() },
                VolumeMount { name: "live".to_string(), mount_path: "/home/kl/.zed_server".to_string(), sub_path: Some(".cache/zed-server".to_string()), ..Default::default() },
                VolumeMount { name: "live".to_string(), mount_path: "/home/kl/.windsurf-server".to_string(), sub_path: Some(".cache/windsurf-server".to_string()), ..Default::default() },
                VolumeMount { name: "live".to_string(), mount_path: "/home/kl/.jetbrains".to_string(), sub_path: Some(".cache/jetbrains".to_string()), ..Default::default() },
                VolumeMount { name: "homecache".to_string(), mount_path: HOME_STATE_DIR.to_string(), sub_path: Some("state".to_string()), ..Default::default() },
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
                VolumeMount { name: "nix".to_string(), mount_path: crate::packages::PROFILE_MOUNT.to_string(), sub_path: Some(format!("var/kloudlite/profiles/{ws_id}")), read_only: Some(true), ..Default::default() },
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
            env: Some(login_env(&spec.name, &spec.owner, ctx.registry_host)),
            resources: Some(quantities(&spec.resources)),
            security_context: Some(hardened()),
            ..Default::default()
        }],
        // Required, not optional, for a seeded workspace: the init container cannot clone without
        // the key.
        volumes: Some({
            let mut v = vec![
                home_volume(ctx.pool, &spec.owner),
                homecache_volume(ctx.pool, &spec.owner),
                workspaces_volume(),
                live_worktree_volume(ctx.pool, id, ws_id),
                // The store, read-only, at its root because the profile lives under it too; the
                // mounts pick the two subdirectories the pod may see.
                host_dir("nix", NIX_ROOT.to_string()),
                attach_volume(ctx.pool, ws_id),
                user_key_volume(init.is_some()),
            ];
            // Only the init container mounts this, so only a seeded workspace needs it at all.
            if init.is_some() {
                v.push(user_key_seed_volume(true));
            }
            if default_image {
                v.extend([ws_ssh_volume(ws_id), keys_volume(ctx.pool, keys_owner(spec))]);
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
    placement(&mut pod_spec, ctx.node_name);
    // `ws_id`, never `id`: for a shared-volume clone `id` is the SOURCE volume, so naming the pod
    // after it makes every clone of one volume claim the same pod name — the clone then adopts its
    // source's running pod, its `podRef` points at another workspace's shell, and the gateway
    // dials THAT on an ssh to the clone. The pod, its ssh host key, its resolv.conf and its
    // profile are all per-WORKSPACE facts; only the worktree's path root is per-volume.
    let mut m = meta(
        ws_id,
        Some(&crate::crd::ws_namespace(&spec.owner, &spec.team)),
        &spec.owner,
        "workspace",
        &ctx.owner_ref,
    );
    // Which workspace this pod IS. Siblings share the namespace, so an attachment grant that named
    // only the namespace would reach all of them; this label is what keeps it to one.
    if let Some(l) = m.labels.as_mut() {
        l.insert(WORKSPACE_LABEL.to_string(), ws_id.to_string());
        // A team pod mounts the TEAM's keys file, and the pod fence (workspace-admission.yaml)
        // admits that path only against this label: the owner label is the person, the file is
        // the team's, and a pod naming a team it does not carry is refused by design.
        let team = keys_owner(spec);
        if team != spec.owner {
            l.insert(TEAM_LABEL.to_string(), team.to_string());
        }
    }
    Ok(Pod { metadata: m, spec: Some(pod_spec), ..Default::default() })
}

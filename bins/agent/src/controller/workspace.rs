//! The `Workspace` reconciler: profile, host key, home, worktree, attachment and the one pod.
//! Split out of `controller.rs` unchanged.

use super::stop::{replicated_condition, running_condition, stop_name, stop_push, StopPush};
use super::{my_node, conditions_eq, create_if_absent, delete_ignoring_404, ensure, heal_labels, owner_ref_of_kind, resolve_volume, settle, stopped_condition, wake_on_finish, write_status, Ctx, Done, Outcome, ReconcileErr, Resolved, RETRY, TICK};
use crate::binding;
use std::time::Duration;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::api::{Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::{Api, Resource, ResourceExt};
use rustic_git_workspaces::crd::{self, DesiredState, VolumeSource};
use rustic_git_workspaces::k8s;
use rustic_git_workspaces::model;
use std::sync::Arc;

/// `ATTACHED_ENV_LABEL` is `spec.attachedEnvironment`'s listing view, same rule as `heal_labels`
/// above: `delete_env`'s sweep selects on it instead of an owner label (a teammate may attach a
/// workspace it does not own), so an object whose label has drifted from spec — the API-authored
/// patch failed to also land, a restored backup — would leave that sweep unable to find it.
async fn heal_attached_label(api: &Api<crd::Workspace>, w: &crd::Workspace) -> Result<(), ReconcileErr> {
    let want = w.spec.attached_environment.as_deref();
    let cur = w.meta().labels.as_ref().and_then(|l| l.get(k8s::ATTACHED_ENV_LABEL)).map(String::as_str);
    if cur == want {
        return Ok(());
    }
    let label = match want {
        Some(env) => serde_json::json!(env),
        None => serde_json::Value::Null,
    };
    let patch = serde_json::json!({"metadata": {"labels": {k8s::ATTACHED_ENV_LABEL: label}}});
    api.patch(&w.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await?;
    Ok(())
}

/// The shared epilogue of every way `ensure_profile` can fail: say what went wrong on status, and
/// then let the pod run anyway if a profile is already on disk (the old tools keep working) or
/// stop the pass with `when` if there is nothing to fall back on.
async fn profile_failed(
    w: &crd::Workspace,
    id: &str,
    gen: i64,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
    (reason, msg): (&str, &str),
    when: Action,
) -> Result<Option<Action>, ReconcileErr> {
    let has = crate::nix::profile_exists(&ctx.profiles_dir, id);
    let st = packages_status(prev, prev.packages.clone(), reason, msg, has, gen);
    write_ws_status_tracking(w, st, prev, ctx).await?;
    Ok(if has { None } else { Some(when) })
}

/// Bring this workspace's Nix profile up to date with `spec.packages`, and say so on status.
/// `None` means the profile is current and the pod may be (re)started; `Some(action)` means
/// status was written and the pass ends here — a build in flight, or a build that failed with
/// no profile to fall back on.
///
/// Runs on EVERY pass, which is what makes packages present after a restore, a clone, a move or
/// an agent restart: each of those arrives with a spec whose hash does not match the profile
/// this node has (or with no profile at all), and the pod is not applied until it does.
///
/// `prev` is advanced as status is written so the pod step below inherits what was said here —
/// a workspace's profile state must not be erased by the pass that goes on to the pod.
async fn ensure_profile(
    w: &crd::Workspace,
    id: &str,
    gen: i64,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    use rustic_git_workspaces::packages;
    // An empty list still builds: the pod mounts `{profiles_dir}/{id}` as a subPath of the
    // READ-ONLY `nix` hostPath, so a missing directory is an unmountable pod, not a pod without
    // extras. An empty
    // `buildEnv` is a cache hit.
    let uid = w.uid().unwrap_or_default();
    // Its own key: a workspace can be pushing (keyed by the Volume's uid) while its profile builds.
    let key = format!("profile:{uid}");

    // A finished build: publish it and record what it is. A running one: say so and wait. The
    // lock is dropped before any await — a `MutexGuard` held across one makes the whole reconcile
    // future non-`Send`, which `Controller::run` refuses.
    let (finished, still_running) = {
        let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(&key) {
            Some((_, h)) if h.is_finished() => (running.remove(&key), false),
            Some(_) => (None, true),
            None => (None, false),
        }
    };
    if still_running {
        let st = packages_status(prev, prev.packages.clone(), "Building", "taking the profile through nix", false, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(Some(Action::requeue(TICK)));
    }

    // Validated again here: the API validates, but an object can be written by kubectl or a
    // restored backup, and a name that is not an attribute must never reach an expression.
    if let Err(e) = packages::validate_list(&w.spec.packages) {
        // Only a spec edit fixes this, and that is an event.
        return profile_failed(w, id, gen, prev, ctx, ("BuildFailed", &e.to_string()), Action::await_change()).await;
    }
    let pin = crate::nix::nixpkgs_pin();
    // The platform's base set first, then the workspace's own, deduplicated: the hash covers
    // both, so rolling the base rebuilds every profile, and a name in both lists is one package.
    let base = crate::nix::base_packages();
    let mut all: Vec<String> = base.clone();
    all.extend(w.spec.packages.iter().filter(|p| !base.contains(p)).cloned());
    if let Err(e) = packages::validate_list(&all) {
        // A bad BASE entry is the operator's mistake, not the user's; the message says which.
        let msg = format!("base packages: {e}");
        return profile_failed(w, id, gen, prev, ctx, ("BuildFailed", &msg), Action::await_change()).await;
    }
    let hash = packages::hash(&pin, &all);
    let observed = crd::PackagesStatus {
        base,
        observed: w.spec.packages.clone(),
        observed_hash: Some(hash.clone()),
        profile: Some(crate::nix::profile_path(&ctx.profiles_dir, id).to_string_lossy().into_owned()),
        nixpkgs: Some(pin.clone()),
    };

    let started_from = ctx.profile_builds.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
    let mut had_finished = false;
    if let Some((_, handle)) = finished {
        let outcome = handle.await.unwrap_or_else(|e| Err(format!("build panicked: {e}")));
        // The spec that build started from, not the one we are looking at now. A PATCH that lands
        // mid-build makes them differ, and publishing then would put yesterday's tools behind
        // today's hash — a workspace that never rebuilds. Drop it and build again below.
        let stale = started_from.as_deref() != Some(hash.as_str());
        match outcome {
            Ok(_) if !stale => {
                tokio::task::spawn_blocking({
                    let id = id.to_string();
                    let profiles = ctx.profiles_dir.clone();
                    let hash = hash.clone();
                    move || {
                        crate::nix::publish(&profiles, &id)?;
                        // Offer it to every other workspace with the same inputs — the store path
                        // is whatever `current` now points at. Best effort on purpose: the profile
                        // is published and correct, so a failure here loses only the sharing, and
                        // failing the reconcile over that would be the worse trade.
                        let indexed = std::fs::read_link(crate::nix::profile_path(&profiles, &id))
                            .and_then(|store_path| crate::nix::record_index(&profiles, &hash, &store_path));
                        if let Err(e) = indexed {
                            tracing::warn!(workspace = %id, error = %e, "built profile not indexed; it will not be reused");
                        }
                        Ok::<(), std::io::Error>(())
                    }
                })
                .await
                .map_err(|e| ReconcileErr(format!("publish panicked: {e}")))?
                .map_err(|e| ReconcileErr(format!("publish profile: {e}")))?;
                had_finished = true;
            }
            Ok(_) => {
                let _ = std::fs::remove_file(crate::nix::building_path(&ctx.profiles_dir, id));
                tracing::info!(workspace = %id, "the spec changed during the build; rebuilding");
            }
            Err(_) if stale => {
                tracing::info!(workspace = %id, "a build for a superseded spec failed; rebuilding");
            }
            Err(e) => {
                // The OLD packages, not the ones that failed (`profile_failed` keeps them):
                // recording the new hash here makes the next pass see hash-match plus a directory
                // on disk and never retry the build.
                let backoff = build_failed_backoff(prev);
                return profile_failed(w, id, gen, prev, ctx, ("BuildFailed", &e), Action::requeue(backoff)).await;
            }
        }
    }

    let current = prev.packages.as_ref().and_then(|p| p.observed_hash.as_deref()) == Some(hash.as_str())
        && crate::nix::profile_exists(&ctx.profiles_dir, id);
    if current {
        return Ok(None);
    }
    // Another workspace on this node already built exactly these inputs. The hash covers the pin,
    // the base set and the spec's packages, so an entry under it IS the store path nix would
    // compute — taking it skips an evaluation of nixpkgs (measured at 28 s cold), not a check.
    //
    // `link_profile` writes the same `.building` path a real build does; that is safe only because
    // this workspace's builds are serialised through `ctx.running` under `profile:{uid}` — the
    // still_running arm above returned before we could get here if one were in flight.
    // Not after our own build: that one just published this exact store path, and the arm below
    // records it as `Built` rather than as something reused.
    if let Some(store_path) = (!had_finished).then(|| crate::nix::indexed(&ctx.profiles_dir, &hash)).flatten() {
        let (profiles, wsid) = (ctx.profiles_dir.clone(), id.to_string());
        tokio::task::spawn_blocking(move || crate::nix::link_profile(&profiles, &wsid, &store_path))
            .await
            .map_err(|e| ReconcileErr(format!("link panicked: {e}")))?
            .map_err(|e| ReconcileErr(format!("link profile: {e}")))?;
        let st = packages_status(prev, Some(observed.clone()), "Built", "reused a profile already on this node", true, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(None);
    }

    // A fresh profile on disk whose hash status does not yet record (the publish above, or a
    // restart between publish and status): record it without building again.
    if had_finished && crate::nix::profile_exists(&ctx.profiles_dir, id) {
        let st = packages_status(prev, Some(observed), "Built", "profile is on disk", true, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(None);
    }

    // A daemon that is not there is not a failed build: it is this node, and it says so under its
    // own reason so the UI does not blame the package list. A workspace that already has a profile
    // still gets its pod — the tools it has keep working while the daemon is down.
    if let Err(e) = ctx.nix.ping().await {
        return profile_failed(w, id, gen, prev, ctx, ("NoNix", &e), Action::requeue(RETRY)).await;
    }

    // Build, on its own thread: `nix` blocks for as long as the substituter takes. The link is
    // made here rather than by `nix -o`: an out-link's auto GC root points at the `.building`
    // path, so the publish rename would orphan it and leave the live profile collectable.
    let expr = packages::expression(&pin, &all);
    let dir = crate::nix::profile_dir(&ctx.profiles_dir, id);
    let building = crate::nix::building_path(&ctx.profiles_dir, id);
    let nix = ctx.nix.clone();
    let timeout = crate::nix::build_timeout();
    // `nix.build` is async (it drives the child through tokio), so this is a plain task; the fs
    // calls after it are a symlink and a mkdir, not the substituter's minutes.
    let handle = tokio::spawn(async move {
        let store_path = nix.build(&expr, timeout).await?;
        // A node that ran the old flat-link layout has `{id}` as a SYMLINK into the store, and
        // `create_dir_all` would happily accept it — every write below then lands inside a
        // read-only store path.
        if dir.is_symlink() {
            std::fs::remove_file(&dir).map_err(|e| format!("old profile link: {e}"))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("profile dir: {e}"))?;
        let _ = std::fs::remove_file(&building);
        std::os::unix::fs::symlink(&store_path, &building).map_err(|e| format!("profile link: {e}"))?;
        Ok(Done { phase: crd::Phase::Ready, ..Done::default() })
    });
    let handle = wake_on_finish(
        handle,
        ctx.wake_workspace.clone(),
        kube::runtime::reflector::ObjectRef::<crd::Workspace>::new(&w.name_any()),
    );
    ctx.profile_builds.lock().unwrap_or_else(|p| p.into_inner()).insert(key.clone(), hash.clone());
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (gen, handle));
    // The OLD packages while it builds, never `observed`: an agent that dies between here and the
    // publish would otherwise leave a status whose hash matches the spec next to the PREVIOUS
    // profile on disk, and the next pass skips the build forever. `observed` is recorded on
    // `Built` and nowhere else — status says what is on the disk, not what is being made.
    let st = packages_status(prev, prev.packages.clone(), "Building", "taking the profile through nix", crate::nix::profile_exists(&ctx.profiles_dir, id), gen);
    write_ws_status_tracking(w, st, prev, ctx).await?;
    Ok(Some(Action::requeue(TICK)))
}

/// How long to wait before retrying a failed build: 60s the first time, growing with how long the
/// workspace has been failing, capped at an hour. A misspelled attribute never becomes buildable
/// on its own — retrying it every minute forever is load on the daemon for nothing, and the fix
/// (a spec edit) is an event that wakes the reconcile regardless of the requeue.
fn build_failed_backoff(prev: &crd::WorkspaceStatus) -> Duration {
    let since = prev
        .conditions
        .iter()
        .find(|c| c.type_ == crd::PACKAGES_READY && c.reason == "BuildFailed")
        .map(|c| k8s_openapi::jiff::Timestamp::now().as_second() - c.last_transition_time.0.as_second())
        .unwrap_or(0);
    Duration::from_secs(since.clamp(60, 3600) as u64)
}

/// Status for the packages step: phase stays what it was (a workspace building a profile is not
/// being CREATED), `observed_generation` stays unset (not converged), the `PackagesReady`
/// condition replaces any earlier one of its type.
fn packages_status(
    prev: &crd::WorkspaceStatus,
    packages: Option<crd::PackagesStatus>,
    reason: &str,
    message: &str,
    ready: bool,
    gen: i64,
) -> crd::WorkspaceStatus {
    let mut conditions: Vec<_> = prev.conditions.iter().filter(|c| c.type_ != crd::PACKAGES_READY).cloned().collect();
    let old = prev.conditions.iter().find(|c| c.type_ == crd::PACKAGES_READY);
    // `lastTransitionTime` is a TRANSITION: a build that fails again for the same reason has not
    // transitioned, and re-stamping it would reset the backoff every pass into a flat 60s retry.
    conditions.push(crd::condition_since(old, crd::PACKAGES_READY, ready && reason == "Built", reason, message, gen));
    crd::WorkspaceStatus { observed_generation: None, packages, conditions, ..prev.clone() }
}

/// Make sure this workspace has an SSH host key, and report its public half on status.
///
/// Get-then-create, never apply: the key is this pod's IDENTITY, pinned in every user's
/// `known_hosts`, so a second generation would look exactly like a man-in-the-middle. The Secret is
/// the record — a pass that finds one reads the public line back out of it and generates nothing.
///
/// Runs before the pod for the same reason `ensure_profile` does: a container started without
/// `/etc/ssh` is an sshd that exits on boot.
async fn ensure_ssh(
    w: &crd::Workspace,
    id: &str,
    ns: &str,
    owner_ref: &OwnerReference,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    use k8s_openapi::api::core::v1::Secret;
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let name = k8s::ws_ssh_secret_name(id);
    let public = match secrets.get_opt(&name).await? {
        Some(s) => s
            .data
            .as_ref()
            .and_then(|d| d.get("ssh_host_ed25519_key.pub"))
            .map(|b| String::from_utf8_lossy(&b.0).trim().to_string())
            // A Secret without the public half is one someone edited: the private key is still the
            // pod's identity, so it is never replaced — status just has nothing to report.
            .unwrap_or_default(),
        None => {
            let (private, public) = crate::sshkeys::generate().map_err(ReconcileErr)?;
            let s = k8s::ws_ssh_secret(id, &w.spec.name, ns, &w.spec.owner, owner_ref, &private, &public);
            match secrets.create(&PostParams::default(), &s).await {
                Ok(_) => public,
                // Lost the race with our own earlier pass: the winner's key is the identity, and
                // the one just generated is discarded unread.
                Err(kube::Error::Api(st)) if st.code == 409 => secrets
                    .get(&name)
                    .await?
                    .data
                    .and_then(|d| d.get("ssh_host_ed25519_key.pub").map(|b| String::from_utf8_lossy(&b.0).trim().to_string()))
                    .unwrap_or_default(),
                Err(e) => return Err(e.into()),
            }
        }
    };
    // ponytail: the Secret is created once and never reconciled, so an existing workspace keeps the
    // `sshd_config` it was made with. Delete the Secret (never the key) and let the next pass
    // rewrite it, or patch just that field here, if the config ever has to change under running
    // workspaces.
    //
    // An empty public half is a hand-edited Secret, not a key: report nothing rather than an empty
    // string the CLI would try to pin — and say so, because the symptom is a workspace nobody can
    // ssh into with no other trace.
    if public.is_empty() {
        tracing::warn!(workspace = %id, secret = %name, "host key Secret has no public half; status.sshHostKey left as it was");
        return Ok(());
    }
    if prev.ssh_host_key.as_deref() != Some(public.as_str()) {
        // `observedGeneration` stays unset: this pass has not converged yet — the pod is still
        // ahead of it.
        let st = crd::WorkspaceStatus { ssh_host_key: Some(public), observed_generation: None, ..prev.clone() };
        write_ws_status_tracking(w, st, prev, ctx).await?;
    }
    Ok(())
}

/// `write_ws_status`, remembering what was written: later steps of the same pass build their status
/// from `prev`, so a write that is not tracked is a condition silently dropped by the next one.
async fn write_ws_status_tracking(
    w: &crd::Workspace,
    st: crd::WorkspaceStatus,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    *prev = st.clone();
    write_ws_status(w, st, ctx).await
}

/// Render this workspace's `/etc/resolv.conf` into the agent-owned attach directory.
///
/// IN PLACE, never via a rename. The pod bind-mounts this file by inode, so replacing it with
/// `rename(2)` — the usual way to write a file atomically — leaves every running pod reading the
/// OLD inode and attachment silently stops working. Verified on a live cluster; do not "fix" this
/// into an atomic write. `std::fs::write` truncates the existing inode, which is what is wanted.
///
/// Truncate-then-write is not atomic, so a lookup landing inside that window reads a short file and
/// fails once. Accepted: the resolver retries, the next write is complete, and the only atomic
/// alternative — rename — is the thing forbidden above.
///
/// Before the pod, never after: the mount is `type: File`, so a missing target is not created —
/// it is a mount failure, and the pod sits in `ContainerCreating` until this file exists.
pub fn write_resolv_conf(pool: &str, ws_id: &str, ws_ns: &str, env_ns: Option<&str>) -> Result<(), ReconcileErr> {
    let dir = k8s::attach_dir(pool, ws_id);
    std::fs::create_dir_all(&dir).map_err(|e| ReconcileErr(format!("attach dir {dir}: {e}")))?;
    let path = k8s::attach_file(pool, ws_id);
    // A pre-migration pod mounted this path with a `subPath`, which kubernetes created as a
    // directory when it did not exist yet. Nothing writes a directory here any more, but a node
    // upgraded from that shape can still have one on disk; clear it rather than leaving the
    // workspace with no DNS for as long as the pod lives.
    if std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
        std::fs::remove_dir_all(&path).map_err(|e| ReconcileErr(format!("attach file {path}: {e}")))?;
    }
    let template = std::fs::read_to_string("/etc/resolv.conf")
        .map_err(|e| ReconcileErr(format!("reading the agent's resolv.conf: {e}")))?;
    std::fs::write(&path, k8s::resolv_conf(&template, ws_ns, env_ns))
        .map_err(|e| ReconcileErr(format!("writing {path}: {e}")))
}

/// The pod step's conditions, keeping whatever the packages step said about this profile — and the
/// `Attached` condition, which is not decoration: it is the ONLY record of which environment's
/// namespace holds this workspace's ingress half, and dropping it on a stop (or any pass that
/// rebuilds the list) strands that grant on the detach after it. The pod path recomputes `Attached`
/// and replaces this copy.
fn ws_conditions(prev: &crd::WorkspaceStatus, ready: Condition) -> Vec<Condition> {
    kept_conditions(&prev.conditions, ready)
}

/// The same, for the writes that have the previous condition list but not the whole status —
/// `resolve_volume` is shared with environments and takes it as a slice, and `settle`'s builders
/// only have what they captured.
///
/// EVERY workspace status write goes through one of these two. Three separate sites that built the
/// list literally each dropped `Attached` and stranded the same grant, which is why the invariant is
/// "no literal condition list on a workspace path" rather than three more fixes.
pub(crate) fn kept_conditions(prev: &[Condition], ready: Condition) -> Vec<Condition> {
    let mut c: Vec<Condition> =
        prev.iter().filter(|c| c.type_ == crd::PACKAGES_READY || c.type_ == crd::ATTACHED).cloned().collect();
    c.push(ready);
    c
}

/// One condition replaced by type, the rest kept in order. `Replicated` is rewritten on every
/// reconcile of a stopped parent, and a naive push would grow the list without bound.
pub(crate) fn replaced(prev: &[Condition], c: Condition) -> Vec<Condition> {
    let mut out: Vec<Condition> = prev.iter().filter(|p| p.type_ != c.type_).cloned().collect();
    out.push(c);
    out
}

/// Drop the dead-node sweep's `Degraded=True/NodeDead` from a parent's conditions.
///
/// The sweep only ever writes it from ANOTHER node, and nothing ever cleared it: a workspace
/// stopped on a node that then died kept `NodeDead` after the node came back Ready, and `/v1`
/// went on answering `start` with 409 "interrupted" forever (drill, 2026-09-03). The owner
/// reconciling its own object IS the proof its node is alive — the watch is field-selected on
/// `status.nodeName`, so nobody else reaches this code for this object.
pub(crate) fn cleared_node_dead(prev: &[Condition]) -> Vec<Condition> {
    prev.iter().filter(|c| !(c.type_ == "Degraded" && c.reason == "NodeDead")).cloned().collect()
}

/// `ws_conditions` with this pass's freshly resolved `Attached` — replacing the preserved copy,
/// which is the previous pass's answer, and dropping it entirely when nothing is attached.
fn with_attached(conds: Vec<Condition>, attached: Option<Condition>) -> Vec<Condition> {
    conds.into_iter().filter(|c| c.type_ != crd::ATTACHED).chain(attached).collect()
}

/// Stop the workspace: cut a final sync point, then delete the pod. The cut is what the wait is
/// for — once it is Ready the worktree's last minute of work exists as a snapshot, and whether any
/// PEER holds a copy of it is the `Replicated` condition's answer, not a gate. The home is on the
/// shared NFS mount and needs no push of its own (spec 2026-09-01).
async fn stop_workspace(
    w: &crd::Workspace,
    prev: crd::WorkspaceStatus,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Action, ReconcileErr> {
    let id = prev.volume_ref.clone().unwrap_or_else(|| w.name_any());
    // Already stopped: the teardown is done, but `Replicated` is not a one-shot fact — a peer
    // catches up minutes later, and the condition is what tells the UI (and the placement rule)
    // that this may now start elsewhere. Recomputed each pass, written only when it actually
    // changed, so a converged workspace is idle.
    if prev.phase == crd::Phase::Stopped {
        let replicated = replicated_condition(ctx, &id, &w.name_any(), replicas_of(ctx, &id), &prev.conditions, gen).await?;
        let conditions = replaced(&cleared_node_dead(&prev.conditions), replicated);
        if prev.observed_generation != Some(gen) || !conditions_eq(&prev.conditions, &conditions) {
            let st = crd::WorkspaceStatus { observed_generation: Some(gen), conditions, ..prev };
            write_ws_status(w, st, ctx).await?;
        }
        return Ok(Action::requeue(TICK));
    }
    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    // The workspace's OWN name, never `id` (which is `volume_ref` — the SOURCE volume for a
    // shared-volume clone). Deleting by `id` here would stop the clone by killing its source's
    // pod, taking a running workspace down with it.
    //
    // Nothing ran, nothing to cut: with no pod there is no writer, so the worktree holds exactly
    // what its last commit or sync point already does. An environment has no equivalent signal —
    // its StatefulSets are scaled to zero by `drain_services` on the way in, so "no pods now" says
    // nothing about whether any ran — and keeps its unconditional cut.
    let cut = prev.pod_ref.is_some();
    if cut {
        match stop_push(&stop_name(w), &w.spec.owner, &id, &w.name_any(), w, crd::SnapshotState::of_workspace(w), ctx).await? {
            StopPush::Landed => {}
            StopPush::Waiting => {
                let conditions = ws_conditions(
                    &prev,
                    crd::condition("Progressing", true, "FlushBeforeStop", "waiting for the final sync point", gen),
                );
                // Deliberately NOT `phase: stopped`: the pod is still up, and observed_generation
                // stays unset so the already-stopped guard above does not swallow the next pass.
                let st = crd::WorkspaceStatus { observed_generation: None, conditions, ..prev };
                write_ws_status(w, st, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
        }
    }
    delete_ignoring_404(&Api::<Pod>::namespaced(ctx.client.clone(), &ns), &w.name_any()).await?;
    // The `stop-{ws}-{gen}` CR is KEPT. It is a transient now, not a commit: `status.head` never
    // names it, so deleting it here would leave the stopped worktree with no sync point anywhere —
    // the last beat's transient was already reclaimed when this one turned Ready, and every
    // replica's `pull_volume` drops a CR-less subvolume within a cycle. A later re-host would then
    // fall all the way back to `head`, losing exactly what the cut above just took.
    //
    // Poke every placeable peer: the cut exists NOW, and waiting out the pull beat is what used to
    // make a cross-node start take minutes. Best-effort by construction — the ticker still comes.
    // Only when there WAS a cut: a workspace that never ran has nothing new for a peer to fetch,
    // so waking the whole fleet would be a cluster-wide listing per no-op stop.
    if cut {
        let live = crate::peer::placeable_nodes(ctx).await;
        crate::peer::wake_peers(ctx, &live, &ctx.peer_secret).await;
    }
    // `ws_conditions`, not a bare vec: a stop that dropped `PackagesReady` left the web
    // showing "installing packages…" for a workspace that is simply off.
    let replicated = replicated_condition(ctx, &id, &w.name_any(), replicas_of(ctx, &id), &prev.conditions, gen).await?;
    let conditions = replaced(&cleared_node_dead(&ws_conditions(&prev, stopped_condition(gen))), replicated);
    let st = crd::WorkspaceStatus {
        phase: crd::Phase::Stopped,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        pod_ref: None,
        conditions,
        ..prev
    };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::requeue(TICK))
}

/// The volume's replica count from the shared watch store, never a GET: a stop must not depend on
/// the Volume being readable (a workspace whose subvolume broke could then never be stopped). An
/// unknown volume gets the CRD's own default, which is what the reconciler that creates the
/// replica children uses for a `Volume` written before the field existed.
fn replicas_of(ctx: &Arc<Ctx>, id: &str) -> u32 {
    ctx.volumes
        .get(&kube::runtime::reflector::ObjectRef::new(id))
        .map(|v| v.spec.replicas)
        .unwrap_or(crd::DEFAULT_REPLICAS)
}

/// A shared-volume clone (`spec.storage.source` is `CloneOf { commit: Some(_), .. }`) checks out
/// a worktree under the SOURCE volume's `live/`, not its own — it owns no `Volume` child, so
/// nothing's ownerReference GC ever reclaims that worktree. An owned-volume workspace needs
/// nothing here: its `Volume`'s own `SUBVOLUME_FINALIZER` deletes the whole voldir, worktree
/// included.
fn is_shared_clone(w: &crd::Workspace) -> bool {
    w.spec
        .storage
        .as_ref()
        .and_then(|s| s.source.as_ref())
        .is_some_and(|src| matches!(src, VolumeSource::CloneOf { commit: Some(_), .. }))
}

fn has_worktree_finalizer(w: &crd::Workspace) -> bool {
    w.metadata.finalizers.as_ref().is_some_and(|fs| fs.iter().any(|f| f == crd::WORKTREE_FINALIZER))
}

/// `WORKTREE_FINALIZER` is added ONLY to a shared-volume clone — an owned workspace never grows
/// one. A finalizer already present from before this workspace's spec stopped reading as a
/// shared clone (a rollback, or a respec) must still be REMOVABLE: the guard is "nothing to add
/// AND nothing to remove", not just "not a clone".
pub async fn reconcile_workspace(w: Arc<crd::Workspace>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    if !is_shared_clone(&w) && !has_worktree_finalizer(&w) {
        return apply_workspace(&w, &ctx).await;
    }
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    finalizer(&api, crd::WORKTREE_FINALIZER, w, |event| async {
        match event {
            FinalizerEvent::Cleanup(w) => cleanup_workspace_worktree(&w, &ctx).await,
            FinalizerEvent::Apply(w) => apply_workspace(&w, &ctx).await,
        }
    })
    .await
    .map_err(|e| ReconcileErr(e.to_string()))
}

/// F5: drop a shared-volume clone's worktree on delete — the only thing that ever reclaims it
/// (see `is_shared_clone`'s doc comment).
pub async fn cleanup_workspace_worktree(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    if is_shared_clone(w) {
        // `volumeRef` names the SOURCE volume (see `resolve_volume`'s `shared` arm); the worktree
        // under it is named by this workspace's own id, same as every checkout call.
        if let Some(volume) = w.status.as_ref().and_then(|s| s.volume_ref.clone()) {
            let (engine, ws_id) = (ctx.engine.clone(), w.name_any());
            tokio::task::spawn_blocking(move || engine.drop_worktree(&volume, &ws_id))
                .await
                .map_err(|e| ReconcileErr(e.to_string()))?
                .map_err(|e| ReconcileErr(e.0))?;
        }
    }
    Ok(Action::await_change())
}

/// Task 7b: a volume claimed on this node may still be on the
/// OLD layout (`live` itself is the single RW subvolume, pre-dating this whole feature) — the
/// pod that's about to mount it needs `live/{volume}` instead. `Engine::migrate_volume` does the
/// physical rename and returns `true` only the one time it actually moved anything; that's the
/// signal to mint the migration-baseline `Snapshot` CR (CR-first, same shape `create_commit` in
/// `api.rs` uses for a normal push) — the EXISTING `reconcile_commit`/`advance_head` machinery
/// then cuts it, marks it Ready and writes `status.head`, so this function only ever needs to run
/// once per volume, not re-implement any of that.
///
/// A worktree named after the volume's own id is exactly what a pre-model workspace already is
/// (workspace id == volume id, module doc in `commit.rs`) and exactly what `checkout`'s
/// `WORKTREE_EXISTS` guard converges on right below this call — so the caller needs no branch for
/// "just migrated" vs. "always was commit-model-native".
///
/// Takes the `Volume` and not its id because the baseline needs an ownerReference to it: a
/// cluster-scoped object may own another cluster-scoped one, so Kubernetes GC reclaims the record
/// with the volume. Without it the baseline outlived every workspace it was cut for — 13 were on
/// the cluster for volumes that no longer exist. Push commits have carried one all along (`api.rs`).
pub(crate) async fn migrate_and_seed_baseline(
    ctx: &Arc<Ctx>,
    vol: &crd::Volume,
    owner: &str,
    state: crd::SnapshotState,
) -> Result<bool, ReconcileErr> {
    let id = &vol.name_any();
    let (engine, vol_id) = (ctx.engine.clone(), id.to_string());
    let migrated = tokio::task::spawn_blocking(move || engine.migrate_volume(&vol_id))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(|e| ReconcileErr(e.0))?;
    if !migrated {
        return Ok(false);
    }
    let api: Api<crd::Snapshot> = Api::all(ctx.client.clone());
    let name = crd::snapshot_name(id);
    let mut snap = crd::Snapshot::new(
        &name,
        crd::SnapshotSpec {
            volume: id.to_string(),
            owner: owner.to_string(),
            worktree: id.to_string(),
            parent: String::new(),
            message: Some("migration baseline".to_string()),
            pinned: false,
            transient: false,
            state: Some(state),
        },
    );
    snap.metadata.labels = Some(crd::commit_labels(owner, id));
    snap.metadata.owner_references = Some(vec![owner_ref_of_kind(vol)?]);
    snap.status = Some(crd::SnapshotStatus { phase: crd::Phase::Working, ready_at: None });
    // Same convergence rule as everything else in this cutover: a retry that finds the CR already
    // there (crash between the rename above landing and this create) is not an error.
    match api.create(&PostParams::default(), &snap).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(true),
        Err(e) => Err(ReconcileErr(e.to_string())),
    }
}

/// `{pool}/homes/{owner}`: on the shared-home NFS mount (`mount_homes` in `lib.rs` puts the export
/// there at agent startup), so materializing an owner's home is plain `mkdir` + `chown` — no
/// subvolume, no snapshot, nothing btrfs-specific. Idempotent; safe on every reconcile.
///
/// Re-verifies the export first and REPAIRS it (`mount_homes`: mounted and answering is a no-op;
/// stale or missing is detach-and-remount) — mkdir under a vanished mount point would build the
/// person an empty home on the node's rootfs and report it Ready, and mkdir under a stale one
/// (the export moved nodes) fails EIO on every reconcile forever without this.
fn ensure_shared_home(pool: &str, export: &str, owner: &str, uid: u32) -> Result<(), String> {
    // Root-gated for the same reason the chown below is: in production the agent is privileged and
    // `/proc/mounts` tells the truth, while a dev/test pool is an ordinary directory nobody ever
    // mounted — checking there would refuse every reconcile.
    if unsafe { libc::geteuid() } == 0 {
        crate::mount_homes(pool, export)?;
    }
    let dir = crate::homes_root(pool).join(owner);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Only root may chown to an arbitrary uid; the agent always runs privileged in production
    // (DaemonSet, see CLAUDE.md), so this only ever no-ops in a dev/test environment, letting the
    // reconcile loop under test exercise the surrounding logic without needing root itself.
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(&dir, Some(uid), Some(uid)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub async fn apply_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    // FIRST, above every write: see `my_node`. A partitioned agent that keeps reconciling erases
    // the sweep's `NodeDead` on the very next tick, which is how `/v1` came to accept `start` on a
    // node the cluster reads as dead.
    let me = my_node(ctx).await;
    if me.dead {
        return Ok(Action::requeue(TICK));
    }
    let gen = w.meta().generation.unwrap_or(0);
    // BEFORE `heal_labels`, and before anything reads the spec: the label patch happens to reject
    // a `/` today, which is the only thing standing between `spec.owner` and a root-run
    // `mkdir`/`chown` under the pool root. Do not rely on a cosmetic call failing first.
    if let Err(why) = model::validate_ws_spec(&w.spec) {
        let prev = w.status.clone().unwrap_or_default();
        return settle(
            Outcome::Permanent(why, "InvalidSpec"),
            w,
            "Workspace",
            gen,
            // `patch_status` is a forced server-side apply: a field omitted here is PRUNED, so an
            // invalid spec would erase the placement memory that says where this workspace lives.
            move |cond| {
                serde_json::json!({
                    "phase": crd::Phase::Error,
                    "nodeName": prev.node_name,
                    "volumeRef": prev.volume_ref,
                    "conditions": kept_conditions(&prev.conditions, cond),
                })
            },
            ctx,
        )
        .await;
    }
    let ws_api = Api::<crd::Workspace>::all(ctx.client.clone());
    heal_labels(&ws_api, w, &w.spec.owner, &w.spec.team, "workspace").await?;
    heal_attached_label(&ws_api, w).await?;
    let mut prev = w.status.clone().unwrap_or_default();
    // Stopping is a home push and a pod delete — it needs neither the disk nor the namespace. Run
    // it BEFORE those gates: a workspace whose Volume failed permanently would otherwise be
    // unstoppable, stuck reporting `creating` with a pod still running on a broken subvolume.
    if w.spec.desired_state == DesiredState::Stopped {
        return stop_workspace(w, prev, gen, ctx).await;
    }
    let vol = match resolve_volume(
        w,
        &w.spec.owner,
        &w.spec.team,
        &w.spec.region,
        &w.spec.storage,
        &prev.node_name.clone(),
        &prev.conditions.clone(),
        gen,
        ctx,
    )
    .await?
    {
        Resolved::Ready(v) => *v,
        Resolved::Settled(a) => return Ok(a),
        // Unobserved on purpose on every wait: this generation has not converged, so the next pass
        // re-runs instead of treating a half-built workspace as done.
        Resolved::Wait { volume_ref, phase, cond, action } => {
            let st = crd::WorkspaceStatus {
                phase,
                observed_generation: None,
                volume_ref: volume_ref.or(prev.volume_ref.clone()),
                conditions: ws_conditions(&prev, cond),
                ..prev
            };
            write_ws_status(w, st, ctx).await?;
            return Ok(action);
        }
    };
    let id = vol.name_any();
    // Starts spread: the owner is alive (it is running this reconcile) and only the owner may give
    // a volume away, so this is the one place the decision can be made at all. Gated on the START
    // pass — this workspace's own status still says `Stopped` — because this function
    // is also the 15s requeue of every workspace on the node, and a cluster-wide sibling listing
    // per workspace per tick is traffic for a decision whose answer is already no. `Stopped` and
    // nothing else, matching the environment's: a workspace parked in `Creating` has no bytes
    // anywhere to spread toward. A listing that could not be completed moves nothing — an unseen
    // sibling may be a running pod.
    if prev.phase == crd::Phase::Stopped {
        if let Some(siblings) = crate::listing::parents_on_volume(ctx, &id).await {
            if let Some(node) = super::stop::start_placement(ctx, &vol, &siblings).await? {
                // Nothing left to do here: this object is unplaced now and `node`'s claim watch
                // picks it up. Await the change rather than requeueing at an object that is no
                // longer ours.
                tracing::info!(workspace = %w.name_any(), %node, "handed over on start");
                return Ok(Action::await_change());
            }
        }
    }
    // The namespace is the OwnerBinding reconciler's to make; this one only waits for it. Creating
    // it here as well is how it ended up with two writers.
    //
    // ponytail: a binding becoming ready wakes a waiting workspace only via its 15s requeue —
    // mapping one binding to every waiting Workspace of that owner is a list per binding event, and
    // the wait is bounded by one tick. Wire a `spec.owner`-indexed reflector if first-workspace
    // latency ever shows up as a complaint.
    if !binding::namespace_ready(ctx, &w.spec.region, &w.spec.owner, &w.spec.team).await? {
        let st = crd::WorkspaceStatus {
            phase: crd::Phase::Creating,
            observed_generation: None,
            volume_ref: Some(id),
            conditions: ws_conditions(
                &prev,
                crd::condition(binding::NAMESPACE_READY, false, "NamespaceNotReady", "waiting for the owner's namespace", gen),
            ),
            ..prev
        };
        write_ws_status(w, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    // The shared home replaces the home Volume (spec 2026-09-01): the agent makes the two mount
    // sources exist before kubelet needs them. `{pool}/homes/{owner}` is NFS — mkdir is the whole
    // materialize. The cache subvolume is local and disposable. Both idempotent, so every reconcile
    // may call them. No WS_HOMES_EXPORT on this node: park, fail closed — a pod started anyway
    // would hostPath an empty local dir and the person's dotfiles would silently not be theirs.
    let Some(export) = ctx.homes_export.as_deref() else {
        let st = crd::WorkspaceStatus {
            phase: crd::Phase::Creating,
            observed_generation: None,
            volume_ref: Some(id),
            conditions: ws_conditions(&prev, crd::condition("Ready", false, "HomeNotReady", "this node has no shared-home mount (WS_HOMES_EXPORT)", gen)),
            ..prev
        };
        write_ws_status(w, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    };
    // `spawn_blocking`, exactly as the `ensure_homecache` call below: `mount_homes` runs
    // `timeout -s KILL 5 ls`, `umount -f -l` and `timeout -s KILL 60 nsenter … mount`, all
    // synchronous — up to ~65 s of a reactor thread that every other workspace on this node shares.
    let (pool, export_owned, owner) = (ctx.pool.clone(), export.to_string(), w.spec.owner.clone());
    tokio::task::spawn_blocking(move || ensure_shared_home(&pool, &export_owned, &owner, k8s::SSH_UID as u32))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(ReconcileErr)?;
    let (engine, owner) = (ctx.engine.clone(), w.spec.owner.clone());
    tokio::task::spawn_blocking(move || engine.ensure_homecache(&owner, k8s::SSH_UID as u32))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(|e| ReconcileErr(e.0))?;

    // Commit-model worktree materialization: a workspace just claimed onto this node (or one
    // whose pod was never started here) has no `live/{id}` subvolume yet. `head` is `None` on a
    // brand-new workspace (bootstrap: an empty worktree) — Task 4 never WRITES `status.head`
    // itself, only preserves whatever is already there via `..prev`; the first writers are Task 5
    // (a commit records the new head) and Task 6 (a clone/restore grafts one on). Until one of
    // those lands, `head == None` is ambiguous between "genuinely bootstrap" and "this workspace's
    // own head just has not been recorded yet" — the guard below tells the two apart the same way
    // the claim itself does, by asking whether the VOLUME has any commits at all.
    // Lazy per-volume migration, before anything mounts (the pod must be recreated to pick up
    // the new path, same as the hostpath cutover) — a no-op every pass after the first.
    migrate_and_seed_baseline(ctx, &vol, &w.spec.owner, crd::SnapshotState::of_workspace(w)).await?;
    // A clone pinned to a commit already knows its head — grafted by the API at clone time,
    // not guessed here — so it never sees `HeadUnknown` and never bootstraps empty next to
    // the source's real history, even on the very first pass.
    let clone_commit = w
        .spec
        .storage
        .as_ref()
        .and_then(|s| s.source.as_ref())
        .and_then(|src| match src {
            VolumeSource::CloneOf { commit: Some(c), .. } => Some(c.as_str()),
            _ => None,
        });
    // Re-host: a node that has never run this worktree checks out its LATEST SYNC POINT in
    // preference to `head`, because the sync beat replicated it after the last commit — the
    // data-loss window on a node death is one `WS_SYNC_SECS`, not everything since the last push.
    // Only when there is no worktree here yet: a live worktree is never swapped under a running
    // pod, whatever the sync points say.
    //
    // Resolved BEFORE the `HeadUnknown` guard below: a transient IS a Snapshot CR, so `has_commits`
    // is true when a sync point is all this volume has, and parking there would strand a workspace
    // that has perfectly good state to start from.
    let synced = if ctx.engine.pool.worktree(&id, &w.name_any()).exists() {
        None
    } else {
        crate::snapshot::latest_transient(ctx, &id, &w.name_any()).await?
    };
    let effective_head = synced.or_else(|| prev.head.clone()).or_else(|| clone_commit.map(str::to_string));
    if effective_head.is_none() && crate::claim::has_commits(ctx, &id).await? {
        // F2 guard: the volume has commits but this workspace's own `head` is still `None` —
        // checking out `None` here would hand it an EMPTY worktree next to real history,
        // which is the never-started-dataless bug in worktree form. Wait for Task 5/6 to
        // write a real head instead of guessing one; zero-commit volumes never reach this arm
        // (`has_commits` is false), so bootstrap is untouched.
        let st = crd::WorkspaceStatus {
            phase: crd::Phase::Creating,
            observed_generation: None,
            volume_ref: Some(id.clone()),
            conditions: ws_conditions(
                &prev,
                crd::condition("Ready", false, "HeadUnknown", "volume has commits but this workspace has no recorded head yet", gen),
            ),
            ..prev
        };
        write_ws_status(w, st, ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    // A clone naming a commit that retention has since swept is wrong forever, not
    // transient: retrying at TICK would spin on the same missing snapshot until someone
    // notices, so this settles Permanent with its own reason distinct from a bad clone
    // SOURCE (`NoSuchSource`, settled earlier in `resolve_volume`/`check_source`).
    // Keyed on `effective_head`, not on `prev.head`: a volume whose only state is a sync point has
    // `prev.head == None` but is going to check that sync point out, never the clone commit — so
    // settling `Permanent/NoSuchCommit` on a swept commit it was never going to use would kill a
    // workspace that has perfectly good state. Only a volume that would ACTUALLY resolve to the
    // clone commit can be permanently broken by that commit being gone.
    if let Some(commit) = clone_commit {
        let phase = if effective_head.as_deref() == Some(commit) {
            crate::claim::commit_phase(ctx, &id, commit).await?
        } else {
            Some(crd::Phase::Ready)
        };
        // The clone's own cut is created by `/v1` microseconds before the clone object itself, so
        // this reconcile almost always arrives while it is still `Working` — the owner has not
        // taken the btrfs snapshot yet. That is one tick away, not wrong forever: settling
        // Permanent here killed every clone at birth. Only an ABSENT (retention-swept, or of
        // another volume) or `Error` commit is permanent.
        if crate::claim::commit_pending(phase) {
            let st = crd::WorkspaceStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                volume_ref: Some(id.clone()),
                conditions: ws_conditions(
                    &prev,
                    crd::condition("Ready", false, "CommitPending", &format!("waiting for snapshot {commit} to be cut"), gen),
                ),
                ..prev
            };
            write_ws_status(w, st, ctx).await?;
            return Ok(Action::requeue(TICK));
        }
        if phase != Some(crd::Phase::Ready) {
            let prev = prev.clone();
            let vref = id.clone();
            return settle(
                Outcome::Permanent(format!("clone commit {commit} is not a ready snapshot of volume {id}"), "NoSuchCommit"),
                w,
                "Workspace",
                gen,
                move |cond| {
                    serde_json::json!({
                        "phase": crd::Phase::Error,
                        "volumeRef": vref,
                        "conditions": ws_conditions(&prev, cond),
                    })
                },
                ctx,
            )
            .await;
        }
    }
    // `WORKTREE_EXISTS` converges a race (this pass and an earlier one both reaching here, or
    // a pod restart finding its own worktree already there) into a no-op rather than an error.
    // The worktree name is the WORKSPACE's own id, never the volume's — the two differ for a
    // shared-volume clone, whose `id` (`volumeRef`) names the SOURCE's volume.
    let (engine, vol_id, ws_id, head) = (ctx.engine.clone(), id.clone(), w.name_any(), effective_head.clone());
    let quota_gb = vol.spec.quota_gb;
    let result = tokio::task::spawn_blocking(move || {
        engine.checkout(&vol_id, head.as_deref(), &ws_id)?;
        // Quota the worktree the instant it exists — waiting for the volume's next reconcile
        // pass would leave a freshly checked-out worktree briefly unquota'd.
        engine.set_quota_worktree(&vol_id, &ws_id, quota_gb)?;
        Ok::<_, rustic_git_workspaces::engine::ops::EngErr>(())
    })
    .await
    .map_err(|e| ReconcileErr(e.to_string()))?;
    match result {
        Ok(()) => {}
        Err(e) if e.0 == rustic_git_workspaces::engine::commit::WORKTREE_EXISTS => {}
        Err(e) => return Err(ReconcileErr(e.0)),
    }
    // First graft: this pass checked out the clone's commit, and nothing else will ever write
    // it as `head` (a clone never gets Task 5's push-time `advance_head` unless it pushes
    // itself) — the preserve pattern, same as `snapshot::advance_head`.
    if prev.head.is_none() {
        if let Some(commit) = clone_commit {
            let prev2 = prev.clone();
            write_ws_status(w, crd::WorkspaceStatus { head: Some(commit.to_string()), ..prev2 }, ctx).await?;
            prev.head = Some(commit.to_string());
        }
    }

    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    let owner_ref = owner_ref_of_kind(w)?;
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &vol.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: ctx.runtime_class.as_deref(),
        default_image: &ctx.default_image,
    };
    // Resolve the attachment before writing anything: a missing or cross-region environment is
    // reported and treated as unattached, never as a half-applied grant.
    let (env, refusal) = match w.spec.attached_environment.as_deref() {
        None => (None, None),
        Some(env_id) => match Api::<crd::Environment>::all(ctx.client.clone()).get_opt(env_id).await? {
            None => (None, Some(("EnvironmentNotFound", format!("environment {env_id} is gone")))),
            // A different region is a different cluster: there is no route and no DNS to grant.
            Some(e) if e.spec.region != w.spec.region => {
                (None, Some(("RegionMismatch", format!("environment {env_id} is in {}", e.spec.region))))
            }
            Some(e) => (Some((crd::env_namespace(env_id), e)), None),
        },
    };
    let env_ns = env.as_ref().map(|(ns, _)| ns.clone());
    // Per-WORKSPACE, like the pod that mounts it: `id` is the shared VOLUME for a clone, and
    // writing this file under the volume's name leaves every clone's pod stuck FailedMount on a
    // resolv.conf that does not exist under its own name.
    // Same rule: `create_dir_all` + `read_to_string` + `write`, on the shared home's NFS mount in
    // the worst case, on every workspace pass.
    let (pool, ws_id, ns_owned, env_ns_owned) =
        (ctx.pool.clone(), w.name_any(), ns.clone(), env_ns.clone());
    tokio::task::spawn_blocking(move || write_resolv_conf(&pool, &ws_id, &ns_owned, env_ns_owned.as_deref()))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))??;
    let policies: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &ns);
    match &env {
        Some((env_ns, e)) => {
            ensure(&policies, &k8s::attach_egress(&ns, &w.name_any(), env_ns, &w.spec.owner, &pod_ctx.owner_ref), ctx).await?;
            // The environment-side half cannot be owned by this Workspace: an ownerReference may
            // not cross namespaces. It is owned by the ENVIRONMENT instead, so deleting the
            // environment collects it, and a detach deletes it by name.
            let env_ref = owner_ref_of_kind(e)?;
            let in_env: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), env_ns);
            // `ws_id`: these policies select the workspace POD by `WORKSPACE_LABEL`, which names
            // the workspace, and siblings share the namespace — keyed by the shared volume a
            // clone's grant would select its source's pod instead of its own.
            ensure(&in_env, &k8s::attach_ingress(env_ns, &ns, &w.name_any(), &w.spec.owner, &env_ref), ctx).await?;
        }
        // Detach is this same pass with the field cleared, so the workspace-side half goes by name.
        None => delete_ignoring_404(&policies, &k8s::attach_policy_name(&w.name_any())).await?,
    }
    // The environment-side half lives in a namespace this spec no longer names, so a detach — or a
    // re-attach to a DIFFERENT environment — would strand it there until that environment is
    // deleted. Which namespace it was in is not lost: the previous pass wrote the environment id
    // into the `Attached` condition's message, and that is where it is read back from. A grant left
    // behind is dormant only until something re-adds an egress with the same workspace id.
    //
    // ponytail: a True condition is the only address kept, so an attach that created the ingress
    // and then died before its status write leaves no record and this pass collects nothing. The
    // environment's own delete collects it; upgrade path is a label on the ingress and a
    // list-by-label sweep in the janitor, if that window ever costs anything.
    let now = env.as_ref().map(|_| w.spec.attached_environment.as_deref().unwrap_or(""));
    let was = prev
        .conditions
        .iter()
        .find(|c| c.type_ == crd::ATTACHED && c.status == "True")
        .map(|c| c.message.clone())
        .filter(|was| now != Some(was.as_str()));
    if let Some(was) = was {
        let old: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), &crd::env_namespace(&was));
        delete_ignoring_404(&old, &k8s::attach_policy_name(&w.name_any())).await?;
    }
    let mut attached = match (&env_ns, &refusal) {
        // The message is the BARE environment id and must stay that: the next pass parses it back
        // out of status to find a grant left in an environment this spec no longer names.
        (Some(_), _) => {
            Some(crd::condition(crd::ATTACHED, true, "Converged", w.spec.attached_environment.as_deref().unwrap_or(""), gen))
        }
        (None, Some((reason, msg))) => Some(crd::condition(crd::ATTACHED, false, reason, msg, gen)),
        // Not attached at all says nothing: an absent condition, not a False one.
        (None, None) => None,
    };
    // Before the pod, never after: a container started on a stale profile is a workspace whose
    // tools silently disagree with its spec.
    if w.spec.desired_state == DesiredState::Running {
        // Per-WORKSPACE, matching the pod's `var/rustic/profiles/{workspace}` subPath: packages are
        // `spec.packages` of THIS workspace, and two clones of one volume may ask for different
        // ones. Keyed by the shared volume, a clone mounts a profile that was never built for it.
        if let Some(action) = ensure_profile(w, &w.name_any(), gen, &mut prev, ctx).await? {
            return Ok(action);
        }
        // Same rule as the profile: the pod mounts this, so it exists first or sshd dies on boot.
        // Same rule: the host key is this WORKSPACE's identity (pinned in the user's known_hosts),
        // and the pod mounts `ws-ssh-{workspace}`. Keying it by the shared volume would give every
        // clone of one volume the same host key AND leave the pod's secret mount unresolvable.
        ensure_ssh(w, &w.name_any(), &ns, &owner_ref, &mut prev, ctx).await?;
    }

    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    // Reality, not intent: a pod created before this feature shipped has no `attach` volume and no
    // `/etc/resolv.conf` mount, and `create_if_absent` never replaces it — so the file this pass
    // wrote and both policies it applied resolve nothing at all. Reporting `Attached=True` there is
    // a lie the user cannot see through, so the live pod decides. An absent pod is not a refusal:
    // the one created below carries the mount.
    if env_ns.is_some() && !pod_carries_the_attach_mount(&pods, &w.name_any()).await? {
        attached = Some(crd::condition(
            crd::ATTACHED,
            false,
            "PodPredatesAttachment",
            "this pod was created before attachment existed and has no resolv.conf mount; stop and start the workspace once",
            gen,
        ));
    }
    let (phase, pod_ref) = match w.spec.desired_state {
        DesiredState::Running => {
            // The seed rides on the VOLUME's source: what the disk was asked to be made from is
            // the one place that answers "does this need cloning", legacy objects included.
            let init = match vol.spec.source.as_ref() {
                None => None,
                Some(s) => {
                    match k8s::git_init_container(s, &ctx.git_init_image, &ctx.git_ssh_host, &ctx.git_ssh_port) {
                        Ok(c) => c,
                        // A name that can never be cloned is permanent, and no pod is started for
                        // it: the alternative is a pod whose init container fails forever.
                        Err(why) => {
                            let prev = prev.clone();
                            return settle(
                                Outcome::Permanent(why, "InvalidSource"),
                                w,
                                "Workspace",
                                gen,
                                move |cond| {
                                    serde_json::json!({
                                        "phase": crd::Phase::Error,
                                        "nodeName": prev.node_name,
                                        "volumeRef": prev.volume_ref,
                                        "conditions": kept_conditions(&prev.conditions, cond),
                                    })
                                },
                                ctx,
                            )
                            .await;
                        }
                    }
                }
            };
            let pod = match k8s::workspace_pod(&w.spec, &id, &w.name_any(), &pod_ctx, init) {
                Ok(p) => p,
                // Unreachable while `validate_ws_spec` runs at the top of this function; kept
                // because the builder is the boundary and must be able to say no on its own.
                Err(why) => {
                    let prev = prev.clone();
                    return settle(
                        Outcome::Permanent(why, "InvalidName"),
                        w,
                        "Workspace",
                        gen,
                        move |cond| {
                            serde_json::json!({
                                "phase": crd::Phase::Error,
                                "nodeName": prev.node_name,
                                "volumeRef": prev.volume_ref,
                                "conditions": kept_conditions(&prev.conditions, cond),
                            })
                        },
                        ctx,
                    )
                    .await;
                }
            };
            create_if_absent(&pods, &pod).await?;
            // Applying a pod is not a pod running. Read it back: a pod can sit Pending on an
            // unschedulable node or CrashLoopBackOff on a bad image, and reporting Ready straight
            // from the apply made a broken workspace indistinguishable from a working one.
            // The pod is named after the WORKSPACE (`k8s::workspace_pod`'s doc): for a shared-volume
            // clone `id` is the source VOLUME, and reading readiness or reporting `podRef` by `id`
            // would point this workspace at its source's pod — the gateway dials `podRef`, so an
            // ssh to the clone would land in the source's shell.
            let pod_name = w.name_any();
            if !pod_is_ready(&pods, &pod_name).await? {
                let st = crd::WorkspaceStatus {
                    phase: crd::Phase::Creating,
                    observed_generation: None,
                    volume_ref: Some(id.clone()),
                    pod_ref: Some(format!("{ns}/{pod_name}")),
                    // `Replicated=False/Running` goes in the SAME write that records the pod:
                    // from the moment a pod exists here, no other node is an option whatever the
                    // copies hold, and a stale `True` left over from the last stop is exactly the
                    // answer placement must never read.
                    conditions: super::with_drain_notice(
                        &prev.conditions,
                        replaced(
                            &with_attached(
                                ws_conditions(&prev, crd::condition("Ready", false, "PodNotReady", "pod is not ready yet", gen)),
                                attached.clone(),
                            ),
                            running_condition(&prev.conditions, gen),
                        ),
                        me.decommissioning,
                        gen,
                    ),
                    ..prev
                };
                write_ws_status(w, st, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
            // `ready`, not `running`: this string is deserialized into `model::WsState` by the
            // `/v1` projection, which spells the running state `Ready`. An unknown phase does not
            // error — it falls back to `Creating`, so a healthy workspace showed "Creating" in the
            // UI forever. `phase_names_the_doc_enum` pins the vocabulary.
            (crd::Phase::Ready, Some(format!("{ns}/{pod_name}")))
        }
        // Handled at the top of this function, before the Volume and namespace gates — stopping IS
        // deleting the pod, and it must not depend on either being healthy.
        DesiredState::Stopped => unreachable!("stopped is handled before the gates"),
    };
    // Same rule as the `PodNotReady` write above: a running workspace is `Replicated=False/Running`
    // for as long as it runs, and `None` for the paths that record no pod at all.
    let conditions = with_attached(
        ws_conditions(&prev, crd::condition("Ready", true, "Converged", "workspace matches spec", gen)),
        attached,
    );
    let conditions = match pod_ref {
        Some(_) => replaced(&conditions, running_condition(&prev.conditions, gen)),
        None => conditions,
    };
    // F7: the drain notice, on the running workspace's own status. The decommission beat used to
    // write it and this very rewrite erased it 15 s later, so the node annotation said `running=1`
    // while the workspace it was waiting on carried nothing at all.
    let conditions = super::with_drain_notice(&prev.conditions, conditions, me.decommissioning, gen);
    let st = crd::WorkspaceStatus {
        phase,
        observed_generation: Some(gen),
        volume_ref: Some(id),
        pod_ref,
        conditions,
        ..prev
    };
    write_ws_status(w, st, ctx).await?;
    Ok(Action::await_change())
}

/// Whether the RUNNING pod can actually see an attachment: it mounts `attach_file` as a hostPath
/// named `"attach"`. A
/// pod that does not exist yet answers `true` — the one this pass is about to create has it, and a
/// pass that reported `PodPredatesAttachment` for an absent pod would flap the condition on every
/// restart.
async fn pod_carries_the_attach_mount(pods: &Api<Pod>, name: &str) -> Result<bool, ReconcileErr> {
    let Some(pod) = pods.get_opt(name).await? else {
        return Ok(true);
    };
    Ok(pod.spec.and_then(|s| s.volumes).is_some_and(|vs| vs.iter().any(|v| v.name == "attach" && v.host_path.is_some())))
}

/// Whether the pod exists AND its `Ready` condition is true. A missing pod is "not ready", never an
/// error: that is the normal state between applying it and the kubelet creating it.
async fn pod_is_ready(pods: &Api<Pod>, name: &str) -> Result<bool, ReconcileErr> {
    let Some(pod) = pods.get_opt(name).await? else {
        return Ok(false);
    };
    Ok(pod
        .status
        .and_then(|s| s.conditions)
        .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True")))
}

pub(crate) async fn write_ws_status(w: &crd::Workspace, st: crd::WorkspaceStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    write_status(w, "Workspace", w.status.as_ref(), &st, ctx, |a, b| {
        a.phase == b.phase
            && a.observed_generation == b.observed_generation
            && a.pod_ref == b.pod_ref
            && a.node_name == b.node_name
            && a.compatible_nodes == b.compatible_nodes
            && a.volume_ref == b.volume_ref
            // `head` in the comparison: without it, a commit's advance of `head` with every other
            // field unchanged reads as a no-op and the write silently never happens — exactly the
            // bug `snapshot::advance_head`'s own test caught.
            && a.head == b.head
            && conditions_eq(&a.conditions, &b.conditions)
    })
    .await
}

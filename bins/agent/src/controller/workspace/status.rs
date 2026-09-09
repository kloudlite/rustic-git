//! What a workspace reconcile writes back besides the pod: the status subresource (with the
//! tracking write that never erases another pass's fields), the healed labels, the rendered
//! resolv.conf for an attachment, and the sshd host key Secret.

use super::*;


/// `ATTACHED_ENV_LABEL` is `spec.attachedEnvironment`'s listing view, same rule as `heal_labels`
/// above: `delete_env`'s sweep selects on it instead of an owner label (a teammate may attach a
/// workspace it does not own), so an object whose label has drifted from spec — the API-authored
/// patch failed to also land, a restored backup — would leave that sweep unable to find it.
pub(crate) async fn heal_attached_label(api: &Api<crd::Workspace>, w: &crd::Workspace) -> Result<(), ReconcileErr> {
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


/// Make sure this workspace has an SSH host key, and report its public half on status.
///
/// Get-then-create, never apply: the key is this pod's IDENTITY, pinned in every user's
/// `known_hosts`, so a second generation would look exactly like a man-in-the-middle. The Secret is
/// the record — a pass that finds one reads the public line back out of it and generates nothing.
///
/// Runs before the pod for the same reason `ensure_profile` does: a container started without
/// `/etc/ssh` is an sshd that exits on boot.
pub(crate) async fn ensure_ssh(
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
            let s = k8s::ws_ssh_secret(id, &w.spec.name, ns, &w.spec.owner, owner_ref, &private, &public, &ctx.registry_host);
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
        tracing::warn!(workspace = %id, name = %name, "workspace.hostkey.missing");
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
pub(crate) async fn write_ws_status_tracking(
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


/// Whether the RUNNING pod can actually see an attachment: it mounts `attach_file` as a hostPath
/// named `"attach"`. A
/// pod that does not exist yet answers `true` — the one this pass is about to create has it, and a
/// pass that reported `PodPredatesAttachment` for an absent pod would flap the condition on every
/// restart.
pub(crate) async fn pod_carries_the_attach_mount(pods: &Api<Pod>, name: &str) -> Result<bool, ReconcileErr> {
    let Some(pod) = pods.get_opt(name).await? else {
        return Ok(true);
    };
    Ok(pod.spec.and_then(|s| s.volumes).is_some_and(|vs| vs.iter().any(|v| v.name == "attach" && v.host_path.is_some())))
}


pub(crate) async fn write_ws_status(w: &crd::Workspace, st: crd::WorkspaceStatus, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    write_status(w, "Workspace", w.status.as_ref(), &st, ctx, |a, b| {
        a.phase == b.phase
            && a.observed_generation == b.observed_generation
            && a.pod_ref == b.pod_ref
            && a.node_name == b.node_name
            && a.volume_ref == b.volume_ref
            // `head` in the comparison: without it, a snapshot's advance of `head` with every other
            // field unchanged reads as a no-op and the write silently never happens — exactly the
            // bug `snapshot::advance_head`'s own test caught.
            && a.head == b.head
            && conditions_eq(&a.conditions, &b.conditions)
    })
    .await
}

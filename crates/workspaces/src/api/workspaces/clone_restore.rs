//! The two grafting verbs: `clone` cuts its own sync point at the moment of the request and
//! reports `based_on`; `restore` grafts onto a named past snapshot and re-attaches the Volume.

use super::*;


#[derive(serde::Deserialize)]
pub(crate) struct CloneBody {
    pub(crate) name: String,
}


/// The one local-copy route.
///
/// It names no node: placement is the claim's job now, and the ONE rule — a node up to date for the
/// SOURCE worktree — is read there. At the instant of the cut above the owner is simply the only
/// node that qualifies, so a running source's clone lands on the owner by arithmetic; there is no
/// "same node" rule here or anywhere.
pub(crate) async fn clone_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CloneBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    check_ws_name(&body.name)?;
    let src = my_ws(&s, &owner, &id).await?;
    refuse_taken_name(kube(&s)?, &owner, &src.spec.team, &body.name).await?;
    // The source's own locks, carried whole: a clone copies a package list, it does not re-pick
    // versions. Normally nothing is left to resolve — only a source written before pins existed
    // has a `@` entry with no lock.
    let locks = lock_for(&s, &src.spec.packages, &src.spec.locks, false).await?;
    let c = kube(&s)?;
    let new_id = rid("ws");
    let volume = ws_volume(&src).ok_or_else(not_ready)?.to_string();
    let quota = storage_quota(c, &src.spec.storage, &volume).await;
    let owner_of = if src.spec.team.is_empty() { owner.name.clone() } else { src.spec.team.clone() };
    // `my_ws` above let a superadmin claim fetch ANY owner's source workspace (list/get, allowed);
    // this is the ALLOCATING step, and that claim must not spend a team's quota it is not a member
    // of. A 404 here matches `my_ws`'s own refusal shape for someone else's workspace.
    if !may_allocate_for(&s, &owner, &owner_of).await {
        return Err(not_found());
    }
    guard_alloc(&s, &owner_of, !src.spec.team.is_empty(), &workspace_cost(quota, &src.spec.resources)).await?;
    // A clone is a second worktree of the SOURCE's own volume, pinned to a cut taken NOW — resolved
    // ONCE, here, so the clone never drifts with the source's later pushes and never lags whatever
    // the last sync beat happened to leave.
    let interrupted = src.status.as_ref().is_some_and(|st| interrupted(&st.conditions));
    let (based_on, cut) =
        clone_base(c, &owner, &volume, &id, interrupted, src.controller_owner_ref(&()), crd::SnapshotState::of_workspace(&src)).await?;
    // An interrupted source is the ONE case that cannot be a second worktree of the source's own
    // volume: that volume is pinned to the node that is down, so the peer holding the cut would
    // settle `Degraded=NodeMismatch` instead of starting. It gets its own volume, seeded from the
    // held cut — see `VolumeSource::SeededFrom`. Every other clone is unchanged.
    let source = if based_on.interrupted {
        VolumeSource::SeededFrom { volume, snapshot: based_on.snapshot.clone() }
    } else {
        VolumeSource::CloneOf { volume, commit: Some(based_on.snapshot.clone()) }
    };
    let w = create_workspace(
        c,
        &new_id,
        crd::WorkspaceSpec {
            owner: owner.name.clone(),
            // A clone lives where its source lives: same team, same namespace.
            team: src.spec.team.clone(),
            name: body.name,
            region: src.spec.region.clone(),
            image: src.spec.image.clone(),
            storage: Some(crd::WorkspaceStorage { quota_gb: quota, source: Some(source) }),
            desired_state: DesiredState::Running,
            resources: Default::default(),
            packages: src.spec.packages.clone(),
            locks,
            attached_environment: None,
        },
    )
    .await?;
    // The cut LAST: the workspace already exists and names it, so nothing can leave a `Working`
    // Snapshot behind that no clone will ever consume and every later clone would 409 on.
    if let Some(snap) = cut {
        let api: Api<crd::Snapshot> = Api::all(c.clone());
        api.create(&PostParams::default(), &snap).await.map_err(kube_err)?;
    }
    let pushed = pushed_volumes(&s, c, &owner).await?;
    Ok(with_based_on(&ws_doc(&w, &pushed), &based_on))
}


#[derive(serde::Deserialize)]
pub(crate) struct RestoreBody {
    name: String,
    // The `snapshot_id` alone is a Snapshot CR name — the old registry-scoped `volume`
    // hint that used to turn a multi-volume scan into one read no longer means anything, since
    // `find_snapshot` looks the CR up by name directly.
    snapshot_id: String,
    // All optional and all overrides: absent means "whatever the snapshot froze", not "the
    // default" — restoring last month's files with today's image is not last month's workspace.
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    packages: Option<Vec<String>>,
    // No `resources` rung on purpose: nothing user-facing offers to size a restore (create and
    // clone both hardcode the default), and an unclamped body field here would let a caller
    // reserve a node's whole capacity. Resources come from the frozen state, then the live
    // source, then the default.
    #[serde(default)]
    quota_gb: Option<u64>,
    #[serde(default)]
    attached_environment: Option<String>,
}


/// New workspace grafted onto an explicit, possibly-older snapshot — a PUSHED snapshot, which is
/// what makes this different from `clone` (always a copy of the current state).
///
/// The snapshot is resolved against the SERVER tier's history, not a live workspace: restoring is
/// most useful precisely when the original is gone, and requiring `my_ws(src)` first is what made
/// a deleted workspace's snapshots unrestorable.
pub(crate) async fn restore_ws(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let c = kube(&s)?;
    check_ws_name(&body.name)?;
    // Restore-to-new IS a clone at a named snapshot: under the snapshot model there is no
    // registry to fetch from any more, so this resolves the request's `snapshot_id` — a `Snapshot`
    // CR name — straight against the CRD, and the new workspace's source becomes
    // `CloneOf{volume, commit: Some(id)}`, exactly `Engine::clone_local_ids`/`checkout`'s own
    // shared-worktree path. `find_snapshot` is the owner check:
    // CR exists, Ready, and the caller may read `spec.owner` — anything else is a 404, same as a
    // missing snapshot, so a caller learns nothing about volumes that are not theirs.
    let snap = find_snapshot(&s, &owner, None, &body.snapshot_id).await?;
    let volume = snap.spec.volume.clone();

    // A `state` from the other kind is a request to refuse, not to half-honour: restoring an
    // environment snapshot as a workspace mounts a database's data directory under the default
    // image with no packages. `None` is a snapshot cut before states existed — "absent means old",
    // and every reader keeps its fallback for it. Checked before any other lookup so the refusal
    // costs nothing beyond the snapshot fetch already made.
    let frozen = match &snap.spec.state {
        Some(crd::SnapshotState::Workspace { image, packages, resources, quota_gb, attached_environment, locks }) => {
            Some((image.clone(), packages.clone(), resources.clone(), *quota_gb, attached_environment.clone(), locks.clone()))
        }
        Some(crd::SnapshotState::Environment { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "this snapshot was cut from an environment; use POST /v1/environments/restore",
            )
                .into_response())
        }
        None => None,
    };

    // A live source still knows its own size and settings; a deleted one gets the standard quota.
    // `my_ws(volume)` resolves only because an OWNED volume shares its parent workspace's id — the
    // one case this can look up. A shared-clone volume's id is the SOURCE workspace's, so this
    // resolves the source, and the team/region a clone contributes below are the source's on
    // purpose: a snapshot taken on a shared worktree has no other owner to ask.
    let src = my_ws(&s, &owner, &volume).await.ok();
    let team = src.as_ref().map(|w| w.spec.team.clone()).unwrap_or_default();
    refuse_taken_name(kube(&s)?, &owner, &team, &body.name).await?;

    // Precedence: the request, then what the snapshot froze, then the live source, then defaults.
    // A snapshot's `state` is DATA — written by an agent, hand-editable in the cluster — so every
    // value it contributes goes through the same checks a request body's does, below.
    let image = body
        .image
        .clone()
        .or_else(|| frozen.as_ref().map(|f| f.0.clone()))
        .or_else(|| src.as_ref().map(|w| w.spec.image.clone()))
        .unwrap_or_else(default_ws_image);
    let packages = body
        .packages
        .clone()
        .or_else(|| frozen.as_ref().map(|f| f.1.clone()))
        .or_else(|| src.as_ref().map(|w| w.spec.packages.clone()))
        .unwrap_or_default();
    crate::packages::validate_list(&packages).map_err(bad_packages)?;
    // Same precedence as `packages` above, from the same source, so the two never disagree about
    // which cut they came from.
    let prev = locks_for(
        frozen
            .as_ref()
            .map(|f| f.5.clone())
            .or_else(|| src.as_ref().map(|w| w.spec.locks.clone()))
            .unwrap_or_default(),
        &packages,
    );
    // The frozen locks are `prev`, so a restore of an unchanged list asks no index at all; an
    // entry the request ADDED is resolved now.
    let locks = lock_for(&s, &packages, &prev, false).await?;
    let resources = frozen
        .as_ref()
        .map(|f| f.2.clone())
        .or_else(|| src.as_ref().map(|w| w.spec.resources.clone()))
        .unwrap_or_default();
    let quota = match (body.quota_gb, &frozen, &src) {
        (Some(q), _, _) => clamp_quota(&s, q),
        (None, Some(f), _) => clamp_quota(&s, f.3),
        (None, None, Some(w)) => storage_quota(c, &w.spec.storage, &volume).await,
        // A deleted source cannot be asked its size, and nothing user-facing offers to name one:
        // someone recovering a lost workspace is not sizing a disk. The standard quota, which is
        // also what `create` sends by default.
        (None, None, None) => FALLBACK_QUOTA_GB,
    };
    // An attachment the caller cannot see is dropped rather than refused: the environment may
    // simply be gone or someone else's now, and that must not make the snapshot unrestorable.
    // `find_env` is the same visibility check `attach_ws` applies.
    let attached_environment = match body.attached_environment.clone().or_else(|| frozen.as_ref().and_then(|f| f.4.clone())) {
        // Only a 404 is "gone, or not mine". An unreachable API server is a 5xx and must be
        // reported as one, not laundered into a silently unattached workspace.
        Some(e) => match find_env(&s, &owner, &e).await {
            Ok(_) => Some(e),
            Err(r) if r.status() == StatusCode::NOT_FOUND => None,
            Err(r) => return Err(r),
        },
        None => None,
    };
    // A restore is an allocation like any other: the snapshot survives the refusal untouched, so
    // the person can raise their quota and try the same id again.
    let owner_of = if team.is_empty() { owner.name.clone() } else { team.clone() };
    // Same reasoning as `clone_ws`: `find_snapshot`/`my_ws` above admit a superadmin claim to READ
    // someone else's history, but that claim must not spend a team's quota it is not a member of.
    if !may_allocate_for(&s, &owner, &owner_of).await {
        return Err(not_found());
    }
    guard_alloc(&s, &owner_of, !team.is_empty(), &workspace_cost(quota, &resources)).await?;
    let new_id = rid("ws");
    let w = create_workspace(
        c,
        &new_id,
        crd::WorkspaceSpec {
            owner: owner.name.clone(),
            team: src.as_ref().map(|w| w.spec.team.clone()).unwrap_or_default(),
            name: body.name,
            // No per-snapshot region under the snapshot model (single-pool, replica-based; cross-
            // region restore is out of scope — see the design doc). A live source still knows its
            // own; for a deleted one the detached Volume holding the bytes does.
            region: match src.as_ref() {
                Some(w) => w.spec.region.clone(),
                None => volume_region(c, &volume).await.unwrap_or_else(|| "default".to_string()),
            },
            image,
            storage: Some(crd::WorkspaceStorage {
                quota_gb: quota,
                source: Some(VolumeSource::CloneOf { volume, commit: Some(body.snapshot_id) }),
            }),
            desired_state: DesiredState::Running,
            resources,
            packages,
            locks,
            attached_environment,
        },
    )
    .await?;
    let pushed = pushed_volumes(&s, c, &owner).await?;
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, &pushed))).into_response())
}

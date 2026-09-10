//! The nix profile a workspace's packages resolve to: built once per input hash, published
//! from the per-node index, its failures backed off, and reported as `PackagesReady`.

use super::*;


/// The shared epilogue of every way `ensure_profile` can fail: say what went wrong on status, and
/// then let the pod run anyway if a profile is already on disk (the old tools keep working) or
/// stop the pass with `when` if there is nothing to fall back on.
pub(crate) async fn profile_failed(
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
pub(crate) async fn ensure_profile(
    w: &crd::Workspace,
    id: &str,
    gen: i64,
    prev: &mut crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    use kloudlite_workspaces::packages;
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

    let pin = crate::nix::nixpkgs_pin(&ctx.settings);
    let base = crate::nix::base_packages(&ctx.settings);
    let inputs = match ProfileInputs::resolve(w, id, pin, base, &ctx.profiles_dir) {
        Ok(i) => i,
        // Only a spec edit fixes any of these, and that is an event.
        Err((reason, msg)) => return profile_failed(w, id, gen, prev, ctx, (reason, &msg), Action::await_change()).await,
    };
    let had_finished = match settle_finished(finished, &key, id, &inputs.hash, prev, ctx).await? {
        Finished::Published => true,
        Finished::Nothing => false,
        Finished::Failed { reason, message, when } => {
            return profile_failed(w, id, gen, prev, ctx, (reason, &message), when).await;
        }
    };
    if reuse_or_record(w, id, gen, prev, &inputs, had_finished, ctx).await? {
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
    // Rendered here only to VALIDATE: a lock whose store path, revision or attribute is not the
    // shape nix writes never reaches an expression, and only a spec edit fixes that. The
    // expression the build actually uses is rendered inside the task below, once every mirror
    // lock has been resolved to a path.
    if let Err(e) = packages::expression(&inputs.pin, &inputs.bare, &inputs.locks) {
        return profile_failed(w, id, gen, prev, ctx, ("BuildFailed", &e.to_string()), Action::await_change()).await;
    }
    let dir = crate::nix::profile_dir(&ctx.profiles_dir, id);
    let building = crate::nix::building_path(&ctx.profiles_dir, id);
    let nix = ctx.nix.clone();
    let timeout = crate::nix::build_timeout(&ctx.settings);
    // `nix.build` is async (it drives the child through tokio), so this is a plain task; the fs
    // calls after it are a symlink and a mkdir, not the substituter's minutes.
    let (pin_c, bare_c, locks_c) = (inputs.pin.clone(), inputs.bare.clone(), inputs.locks.clone());
    let handle = tokio::spawn(async move {
        // Every lock's bytes are pulled from the cache BEFORE the build, which is what keeps a
        // pinned package from being compiled here: the build itself only realises the `buildEnv`
        // symlink tree. Sequential — one substituter, and a parallel pull only moves the wait.
        let mut resolved = Vec::with_capacity(locks_c.len());
        for l in locks_c {
            // A mirror lock carries a revision instead of a path, so ask nix what it evaluates to
            // and treat the answer as any other lock. Resolving it HERE rather than leaving the
            // `getFlake` in the build expression is the point: an expression that evaluated the
            // foreign nixpkgs would happily build what it found there from source.
            let path = if l.store_path.is_empty() {
                nix.eval_out_path(&l.rev, &l.attr_path, timeout)
                    .await
                    .map_err(|e| format!("{} ({}): {e}", l.entry, l.version))?
            } else {
                l.store_path.clone()
            };
            if let Err(e) = nix.copy_from_cache(&path, timeout).await {
                return Err(if e.starts_with(crate::nix::NOT_CACHED) {
                    format!("{}: {} ({}) is not in {}", crate::nix::NOT_CACHED, l.entry, l.version, crate::nix::CACHE)
                } else {
                    format!("{} ({}): {e}", l.entry, l.version)
                });
            }
            resolved.push(crd::Lock { store_path: path, ..l });
        }
        let expr = packages::expression(&pin_c, &bare_c, &resolved).map_err(|e| e.to_string())?;
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
    ctx.profile_builds.lock().unwrap_or_else(|p| p.into_inner()).insert(key.clone(), inputs.hash.clone());
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (gen, handle));
    // The OLD packages while it builds, never `observed`: an agent that dies between here and the
    // publish would otherwise leave a status whose hash matches the spec next to the PREVIOUS
    // profile on disk, and the next pass skips the build forever. `observed` is recorded on
    // `Built` and nowhere else — status says what is on the disk, not what is being made.
    let st = packages_status(prev, prev.packages.clone(), "Building", "taking the profile through nix", crate::nix::profile_exists(&ctx.profiles_dir, id), gen);
    write_ws_status_tracking(w, st, prev, ctx).await?;
    Ok(Some(Action::requeue(TICK)))
}

/// Everything a build is keyed and rendered from: the pinned nixpkgs, the bare list the
/// expression evaluates, the locks the list still asks for, the hash, and the status that will
/// be recorded as `Built`. Pure — the settings and the profile dir come in as arguments — so
/// every refusal shape is a unit test. `Err` is the condition reason and message to park on.
#[derive(Debug)]
pub(crate) struct ProfileInputs {
    pub(crate) pin: String,
    pub(crate) bare: Vec<String>,
    pub(crate) locks: Vec<crd::Lock>,
    pub(crate) hash: String,
    pub(crate) observed: crd::PackagesStatus,
}

impl ProfileInputs {
    pub(crate) fn resolve(
        w: &crd::Workspace,
        id: &str,
        pin: String,
        base: Vec<String>,
        profiles_dir: &std::path::Path,
    ) -> Result<ProfileInputs, (&'static str, String)> {
        use kloudlite_workspaces::packages;
    // Validated again here: the API validates, but an object can be written by kubectl or a
    // restored backup, and a name that is not an attribute must never reach an expression.
    if let Err(e) = packages::validate_list(&w.spec.packages) {
        // Only a spec edit fixes this, and that is an event.
        return Err(("BuildFailed", e.to_string()));
    }
        // The platform's base set first, then the workspace's own, deduplicated: the hash covers
    // both, so rolling the base rebuilds every profile, and a name in both lists is one package.
        // Only `spec.packages` is ever resolved — `/v1` writes no lock for a base entry — so a pinned
    // base entry could never be built and is the operator's mistake, reported as one.
    if let Some(p) = base.iter().find(|p| p.contains('@')) {
        let msg = format!("base packages: {p} is pinned to a version; base packages carry none");
        return Err(("BuildFailed", msg));
    }
    let mut all: Vec<String> = base.clone();
    all.extend(w.spec.packages.iter().filter(|p| !base.contains(p)).cloned());
    if let Err(e) = packages::validate_list(&all) {
        // A bad BASE entry is the operator's mistake, not the user's; the message says which.
        let msg = format!("base packages: {e}");
        return Err(("BuildFailed", msg));
    }
    // A pinned entry is built from its LOCK, never from the pinned nixpkgs, so it leaves the list
    // the expression evaluates and comes back as a store path (or a revision) below.
    let bare = packages::bare(&all);
    // Only the locks the LIST still asks for. A lock outlives the entry that made it — dropping
    // `nodejs@20` from `spec.packages` leaves its lock behind until the next resolve — and a stale
    // one in the hash or the expression would keep installing a package nobody asked for.
    let locks: Vec<crd::Lock> = w.spec.locks.iter().filter(|l| w.spec.packages.contains(&l.entry)).cloned().collect();
    // `/v1` writes a lock for every `@` entry it accepts. One missing means the object did not
    // come through `/v1` — a restored backup, a `kubectl edit` — and guessing a version here is
    // the one thing this design refuses: say so and wait for a spec that carries the answer.
    if let Some((raw, _)) = packages::pinned(&w.spec.packages).into_iter().find(|(raw, _)| !locks.iter().any(|l| &l.entry == raw)) {
        let msg = format!("{raw} has no resolved version; set the packages again to resolve it");
        return Err((crd::PKG_UNRESOLVED, msg));
    }
    let hash = packages::hash(&pin, &bare, &locks);
    let observed = crd::PackagesStatus {
        base,
        observed: w.spec.packages.clone(),
        observed_hash: Some(hash.clone()),
        profile: Some(crate::nix::profile_path(profiles_dir, id).to_string_lossy().into_owned()),
        nixpkgs: Some(pin.clone()),
        // The observed half of `spec.locks`: reported alongside the profile so a reader can tell a
        // lock the node has actually built from one the api only just wrote.
        locked: locks
            .iter()
            .map(|l| crd::LockedStatus {
                entry: l.entry.clone(),
                version: l.version.clone(),
                rev: l.rev.clone(),
            })
            .collect(),
    };
    Ok(ProfileInputs { pin, bare, locks, hash, observed })
    }
}

/// What a build that has finished leaves behind.
enum Finished {
    /// Published under the current hash: the reuse arms record it.
    Published,
    /// Nothing finished, or the build was stale and is simply dropped: build again.
    Nothing,
    /// Failed on the current inputs: park the workspace with this reason.
    Failed { reason: &'static str, message: String, when: Action },
}

/// A finished build is published under the hash it STARTED from; one that started from an
/// older spec is dropped and rebuilt; a failure is reported with its own reason.
async fn settle_finished(
    finished: Option<(i64, tokio::task::JoinHandle<Result<Done, String>>)>,
    key: &str,
    id: &str,
    hash: &str,
    prev: &crd::WorkspaceStatus,
    ctx: &Arc<Ctx>,
) -> Result<Finished, ReconcileErr> {
    let started_from = ctx.profile_builds.lock().unwrap_or_else(|p| p.into_inner()).remove(key);
    let mut had_finished = false;
    if let Some((_, handle)) = finished {
        let outcome = handle.await.unwrap_or_else(|e| Err(format!("build panicked: {e}")));
        // The spec that build started from, not the one we are looking at now. A PATCH that lands
        // mid-build makes them differ, and publishing then would put yesterday's tools behind
        // today's hash — a workspace that never rebuilds. Drop it and build again below.
        let stale = started_from.as_deref() != Some(hash);
        match outcome {
            Ok(_) if !stale => {
                tokio::task::spawn_blocking({
                    let id = id.to_string();
                    let profiles = ctx.profiles_dir.clone();
                    let hash = hash.to_string();
                    move || {
                        crate::nix::publish(&profiles, &id)?;
                        // Offer it to every other workspace with the same inputs — the store path
                        // is whatever `current` now points at. Best effort on purpose: the profile
                        // is published and correct, so a failure here loses only the sharing, and
                        // failing the reconcile over that would be the worse trade.
                        let indexed = std::fs::read_link(crate::nix::profile_path(&profiles, &id))
                            .and_then(|store_path| crate::nix::record_index(&profiles, &hash, &store_path));
                        if let Err(e) = indexed {
                            tracing::warn!(workspace = %id, error = %e, "profile.index.failed");
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
                tracing::info!(workspace = %id, reason = "spec-changed", "workspace.rebuilding");
            }
            Err(_) if stale => {
                tracing::info!(workspace = %id, reason = "superseded", "workspace.rebuilding");
            }
            Err(e) => {
                // A path the public cache does not have is not a broken list: it is a version this
                // node cannot install without building it, which `--max-jobs 0` refuses. Its own
                // reason, so the web can say which entry and offer an update rather than "failed".
                let reason = if e.starts_with(crate::nix::NOT_CACHED) { crd::PKG_NOT_CACHED } else { "BuildFailed" };
                // The OLD packages, not the ones that failed (`profile_failed` keeps them):
                // recording the new hash here makes the next pass see hash-match plus a directory
                // on disk and never retry the build.
                // A cache that does not hold the path will not hold it a minute from now either:
                // only a spec edit (a different version) fixes it, and that is an event.
                let when = if reason == crd::PKG_NOT_CACHED {
                    Action::await_change()
                } else {
                    Action::requeue(build_failed_backoff(prev))
                };
                return Ok(Finished::Failed { reason, message: e, when });
            }
        }
    }
    Ok(if had_finished { Finished::Published } else { Finished::Nothing })
}

/// The three ways a profile can already be here — recorded, indexed from a sibling with the
/// same inputs, or freshly published — each written back as `Built`. `true` means there is
/// nothing to build.
async fn reuse_or_record(
    w: &crd::Workspace,
    id: &str,
    gen: i64,
    prev: &mut crd::WorkspaceStatus,
    inputs: &ProfileInputs,
    had_finished: bool,
    ctx: &Arc<Ctx>,
) -> Result<bool, ReconcileErr> {
    let current = prev.packages.as_ref().and_then(|p| p.observed_hash.as_deref()) == Some(inputs.hash.as_str())
        && crate::nix::profile_exists(&ctx.profiles_dir, id);
    if current {
        // Built and recorded, but the CONDITION can still say `Building`: a workspace that moved
        // nodes mid-build carries the old node's `Building` with this same hash, and this node
        // found the profile in its index without building. The tools are there; say so, or the
        // UI shows a build that never ends.
        let ready = prev.conditions.iter().any(|c| c.type_ == crd::PACKAGES_READY && c.status == "True");
        if !ready {
            let st = packages_status(prev, Some(inputs.observed.clone()), "Built", "profile is on disk", true, gen);
            write_ws_status_tracking(w, st, prev, ctx).await?;
        }
        return Ok(true);
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
    if let Some(store_path) = (!had_finished).then(|| crate::nix::indexed(&ctx.profiles_dir, &inputs.hash)).flatten() {
        let (profiles, wsid) = (ctx.profiles_dir.clone(), id.to_string());
        tokio::task::spawn_blocking(move || crate::nix::link_profile(&profiles, &wsid, &store_path))
            .await
            .map_err(|e| ReconcileErr(format!("link panicked: {e}")))?
            .map_err(|e| ReconcileErr(format!("link profile: {e}")))?;
        let st = packages_status(prev, Some(inputs.observed.clone()), "Built", "reused a profile already on this node", true, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(true);
    }

    // A fresh profile on disk whose hash status does not yet record (the publish above, or a
    // restart between publish and status): record it without building again.
    if had_finished && crate::nix::profile_exists(&ctx.profiles_dir, id) {
        let st = packages_status(prev, Some(inputs.observed.clone()), "Built", "profile is on disk", true, gen);
        write_ws_status_tracking(w, st, prev, ctx).await?;
        return Ok(true);
    }
    Ok(false)
}


/// How long to wait before retrying a failed build: 60s the first time, growing with how long the
/// workspace has been failing, capped at an hour. A misspelled attribute never becomes buildable
/// on its own — retrying it every minute forever is load on the daemon for nothing, and the fix
/// (a spec edit) is an event that wakes the reconcile regardless of the requeue.
pub(crate) fn build_failed_backoff(prev: &crd::WorkspaceStatus) -> Duration {
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
pub(crate) fn packages_status(
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

#[cfg(test)]
mod inputs_tests {
    use super::*;

    fn ws(packages: &[&str], locks: Vec<crd::Lock>) -> crd::Workspace {
        crd::Workspace::new(
            "ws-1",
            crd::WorkspaceSpec {
                owner: "alice".into(),
                team: String::new(),
                name: "w".into(),
                region: "r".into(),
                image: String::new(),
                storage: None,
                desired_state: DesiredState::Running,
                resources: crd::PodResources::default(),
                packages: packages.iter().map(|p| p.to_string()).collect(),
                locks,
                attached_environment: None,
            },
        )
    }

    fn lock(entry: &str) -> crd::Lock {
        crd::Lock { entry: entry.into(), version: "20.1.0".into(), attr_path: "nodejs".into(), rev: "abc".into(), store_path: "/nix/store/x".into(), resolved_at: String::new(), source: crd::LockSource::Nixhub }
    }

    /// The hash covers the pin, the base set and the spec; a lock for an entry the list no longer
    /// asks for is dropped before it can reach the hash or the expression.
    #[test]
    fn inputs_keep_only_the_locks_the_list_still_asks_for() {
        let w = ws(&["ripgrep", "nodejs@20"], vec![lock("nodejs@20"), lock("python@3")]);
        let i = ProfileInputs::resolve(&w, "ws-1", "pin".into(), vec!["git".into()], std::path::Path::new("/p")).unwrap();
        assert_eq!(i.locks.iter().map(|l| l.entry.as_str()).collect::<Vec<_>>(), ["nodejs@20"]);
        assert_eq!(i.bare, vec!["git".to_string(), "ripgrep".to_string()]);
        assert_eq!(i.observed.locked.len(), 1);
        assert!(!i.hash.is_empty());
    }

    /// A pinned entry the api never locked is not guessed at: its own reason, and it waits.
    #[test]
    fn a_pin_without_a_lock_is_unresolved_not_built() {
        let w = ws(&["nodejs@20"], vec![]);
        let err = ProfileInputs::resolve(&w, "ws-1", "pin".into(), vec![], std::path::Path::new("/p")).unwrap_err();
        assert_eq!(err.0, crd::PKG_UNRESOLVED);
        assert!(err.1.contains("nodejs@20"), "{}", err.1);
    }

    /// A pinned BASE entry is the operator's mistake, reported as one rather than as the user's.
    #[test]
    fn a_pinned_base_entry_is_refused_as_a_build_failure() {
        let w = ws(&[], vec![]);
        let err = ProfileInputs::resolve(&w, "ws-1", "pin".into(), vec!["nodejs@20".into()], std::path::Path::new("/p")).unwrap_err();
        assert_eq!(err.0, "BuildFailed");
        assert!(err.1.starts_with("base packages:"), "{}", err.1);
    }
}


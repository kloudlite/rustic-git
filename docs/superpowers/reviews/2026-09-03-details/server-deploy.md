# Server tier and deploy — review, 2026-09-03

Scope: `bins/{server,worker,gateway,kl}`, `crates/{core,storage,gitbase,pulls,app,git,registry}`,
`deploy/` (AKS yamls + `deploy/k3s/*`), `tests/`, `.github/workflows`. Read-only. Tree at
`4c7e94c9`. Explicitly out of scope: `crates/workspaces`, `bins/agent`, `bins/api`, `web` — except
where the deploy manifests grant them authority, which is reviewed here.

Method: read `docs/superpowers/reviews/2026-09-02-codebase-review.md` and its details, grepped each
in-scope finding's fix in the tree, then read `git diff 684aecaf HEAD` over these paths, then read
the manifests fresh against the verbs the code actually calls.

**Totals: Critical 0 · Important 3 · Minor 7 · Cleanup 2.**

The load-bearing rules in CLAUDE.md were re-checked and still hold. `BROWSE_TAILS` is consistent
in both directions and machine-enforced (see "good, leave alone"). No new correctness bug was found
in the server tier since 684aecaf; the diff over these paths is 14 files, and 11 of them are the
deletion of the dead volume-delete routes plus image repins.

---

## Did the 2026-09-02 findings land?

Every in-scope finding, grepped:

| # | Finding | Landed | Evidence |
|---|---|---|---|
| High 3 | ZeroFS NFS behind a NetworkPolicy | yes | `deploy/k3s/system-netpol.yaml` (new file), `zerofs-nfs-from-agents`, 2049 from node/flannel /32s only |
| High 4 | upload-pack negotiation cap | yes | `bins/server/src/router/git.rs:121-146` `MAX_NEGOTIATION`; test `tests/git_http_limits.rs:108` incl. the negative case at `:142` |
| High 1 | `wt-` in the agent admission policy | yes | `agent-admission.yaml` — `startsWith('wt-')` in all three branches |
| High 2 | policy 2 covers pods/statefulsets/services/networkpolicies/limitranges | yes (CREATE/UPDATE only — see Important 1) | `agent-admission.yaml` second policy's resourceRules |
| High 7 | gVisor required by admission | yes | `workspace-admission.yaml:90` |
| Med | `capabilities.add` fenced | yes | `workspace-admission.yaml:96-101` |
| Med | hostPath allow-list narrowed | partly | `workspace-admission.yaml:70-83` — owner-scoped for `homes`/`homecache`, still any tenant's under `/wspool-prod/vol/` (Minor 6) |
| Med | api-rbac Secret `resourceNames` | yes | `api-rbac.yaml` — `["user-key"]`, with `create` split into its own rule and the reasoning written down |
| Med | `kl` really pins the host key | yes | `bins/kl/src/config.rs` `pin_host_key` refuses a changed key, `KL_ACCEPT_NEW_HOST_KEY` escape hatch |
| Med | `harden-node.sh` blanket cni0/pod-CIDR accept | yes | `harden-node.sh:57-75` — the accept is gone, with a "never widen back" note |
| Med | repo description newline forgery | yes | `crates/api/src/repos.rs:60`, tests at `:572-573` |
| Med | claim amplification from anonymous callers | yes | `crates/app/src/lib.rs:434-470` — `empty_prefix` gate + leader read that writes nothing; residual marked `ponytail:` |
| Med | `/metrics` on the peer listener | yes | `bins/server/src/router/mod.rs:30` — own listener (`KLOUDLITE_METRICS_ADDR`) |
| Med | `max_layer` default vs the 5 GiB copy cap | yes | `crates/registry/src/blobs.rs:20` `DEFAULT_MAX_LAYER = 5 GiB` |
| Med | blob-row backfill lock / not-held on error | yes | `crates/registry/src/store.rs:229-262` — `keyed_lock`, re-read under the lock, incomplete walk withholds the mark |
| Med | unbounded `join_all` fan-out | yes | `manifests.rs:199` and `store.rs:268` use `buffered(STAT_CONCURRENCY)` |
| Med | PR `base`/`head` ref validation | yes | `browse_api/pulls.rs:147-181` `valid_branch` |
| Med | stale mergeability verdict | yes | `browse_api/pulls.rs:568-575` — 409 on mismatched tips; the leniency for unstamped verdicts is marked `ponytail:` |
| Med | `agent-peer.yaml` 9464 | yes | `agent-peer.yaml:42-49` |
| Med | `alpine/git`, `zerofs` mutable tags | yes | both `@sha256` pinned (`zerofs.yaml:112`) |
| Med | zerofs memory limit | yes | `zerofs.yaml:120-129`, with the reason the limit is 2Gi not 1Gi |
| Med | agent-rbac header table miscounts | yes | now "all six" `/status`, "all four" `/finalizers` |

Nothing in scope regressed. The one item whose *stated ceiling* has since gone stale is
Important 1 below.

---

## Important

### 1. The agent admission policy still does not cover DELETE, and `services: delete` made that residual bigger than its own comment claims

`deploy/k3s/agent-admission.yaml:88-92` (the header) and `:96-118` (resourceRules);
`deploy/k3s/agent-rbac.yaml` `services` rule.

`kloudlite-agent-tenant-namespaces-only` matches `operations: ["CREATE", "UPDATE"]` only. The
header states the accepted residual: *"the residual is denial of service on a kube-system pod the
kubelet recreates — not a privilege gain like CREATE is."* Commit `4c7e94c9` added
`services: ["create","patch","delete"]` to the ClusterRole. A Service is not recreated by any
kubelet: deleting `kube-dns` in `kube-system`, or the `kubernetes` Service in `default`, is a
cluster-wide durable outage that survives until someone reapplies k3s's bundled manifests. The
same is true of the pre-existing cluster-wide `networkpolicies: delete` (delete the tenant egress
policies and the isolation model is gone) and `statefulsets: delete`.

Yes, the agent is already root on its node, and the file says so. The finding is not that the
verb is wrong — it is that the written ceiling is now false, and the fix is four lines.

Fix (cost S): `oldObject` IS populated on DELETE (`object` is null), so add one resourceRule and
one branch:

```yaml
      - apiGroups: [""]
        apiVersions: ["v1"]
        operations: ["DELETE"]
        resources: ["services", "pods"]
      - apiGroups: ["apps"]
        apiVersions: ["v1"]
        operations: ["DELETE"]
        resources: ["statefulsets"]
      - apiGroups: ["networking.k8s.io"]
        apiVersions: ["v1"]
        operations: ["DELETE"]
        resources: ["networkpolicies"]
```

and, ahead of the existing expression in `validations`:

```yaml
    - expression: >-
        request.operation != 'DELETE'
          || oldObject.metadata.namespace.startsWith('ws-')
          || oldObject.metadata.namespace.startsWith('wt-')
          || oldObject.metadata.namespace.startsWith('env-')
      message: "kloudlite-agent may only delete namespaced objects in ws-/wt-/env- namespaces"
```

(The existing expression dereferences `object.kind`, which is null on a DELETE, so the new rule
must be a SEPARATE validation entry and the existing one must be left matching CREATE/UPDATE — do
not fold the two together.) Then rewrite the header's "Not covered, deliberately: DELETE"
paragraph, since it no longer is.

### 2. Dead RBAC grant: `apps/deployments: get, delete`

`deploy/k3s/agent-rbac.yaml` (`deployments` rule, and the `deployments (apps)` row in the header
table, "legacy migration only").

`grep -rn "Deployment" bins/ crates/ --include=*.rs` returns nothing outside comments — the type
is not imported anywhere in the tree; the legacy StatefulSet migration it existed for is gone.
The header table is documented as *being* the role ("a verb that is not in the table is not in the
rules"), so a stale row is exactly the kind of drift that file is designed to prevent.

Note the near-miss that hides it: `bins/agent/src/controller/environment.rs:464`
`async fn deployment_status(deployments: &Api<StatefulSet>, ...)` — the name still says Deployment
but the type is a StatefulSet, which is why a grep for the word looks like it has hits.

Fix (cost XS): delete the rule and the table row; rename `deployment_status` → `service_status_of`
(out-of-scope path, mention it to whoever owns the agent).

### 3. `tests/routing.rs` forwarding tests flake under load — the claims expire mid-test

`tests/routing.rs:26-96` (`node()`), and every test that calls `app.claim()` then does something
slow before asserting `warm_count()` — the worst are
`a_real_git_push_and_clone_work_through_a_forwarding_node:393`,
`a_real_ssh_clone_works_through_a_forwarding_node:1080`, `a_push_after_a_stray_opener_succeeds`,
and `an_unhealthy_node_stops_serving_but_still_forwards:663`.

Root cause, not symptom: `ownership::LEASE_TTL` is 10 s wall-clock
(`crates/storage/src/ownership/mod.rs:17`). In production `bins/server/src/lanes.rs:36-49` renews
every held lease on a beat. `node()` deliberately runs **one** `election_tick()` and **no loop**
("renewal cadence is lanes.rs's, not what these tests prove"), so a claim taken at t=0 is dead at
t=10 s and nothing renews it. `App::route_for` filters expired entries
(`crates/app/src/lib.rs:412` `filter(|e| !ownership::is_expired(e, now))`), so once it lapses the
forwarding node B takes the claim path instead of the forward path, opens the repo itself, and the
assertions invert: `assert_eq!(b.store.pool.warm_count(), 0)` fails, or A's `1` becomes `0`.

Ten seconds is not a lot on a loaded machine when the test in between does
`git init` + write 3 MiB + `git commit` + `git push` + `git clone` through three in-process axum
servers, all of it competing with `cargo test`'s other integration binaries on the same box. That
is exactly the "only under load" signature.

Fix (cost S), two halves, both needed:

1. Give `node()` the renewal beat production has, so a lease held across a long operation survives
   it. At the end of `node()`, beside the existing `election_tick`:

   ```rust
   // Production renews on a beat (`lanes.rs`); without it a claim taken here is dead in
   // LEASE_TTL and a forwarding test that runs longer than that silently becomes a claiming test.
   let a5 = app.clone();
   tokio::spawn(async move {
       loop {
           tokio::time::sleep(std::time::Duration::from_secs(3)).await;
           let _ = a5.election_tick().await;
           let _ = a5.renew_once().await;
       }
   });
   ```

2. `renew_once` renews `pool.warm_repos()` — repos this node has OPEN — so it does not cover the
   window between `a.app.claim(&repo)` and A's first forwarded request, which is where the git
   fixture work sits. In the four tests above, move the fixture setup (tempdir, `git init`, the
   3 MiB write, `git commit`) to BEFORE the `claim()` call, so the claim is immediately followed by
   the network work that opens the repo. Two of them already do this; `:393` and `:1080` do not.

Do not "fix" this by widening `LEASE_TTL` or by re-claiming before the assertion — the first
changes production behaviour to suit a test, the second hides the very lapse the test would
otherwise catch.

---

## Minor

4. **First-party images are pinned by commit-SHA tag, not by digest.** `deploy/kloudlite.yaml:58,398,579`,
   `deploy/k3s/agent-daemonset.yaml:114`, `deploy/k3s/gateway.yaml:131`,
   `deploy/kloudlite-web.yaml:49`. Third-party images in the same files *are* digest-pinned
   (`zerofs.yaml:112`, the `alpine/git` init image), so the asymmetry is now the odd one out. A
   GHCR tag is mutable; a re-pushed `:<sha>` silently changes what a `IfNotPresent` node pulls next.
   Fix (cost S): have `deploy/pin.sh` resolve each SHA to a digest
   (`docker buildx imagetools inspect --format '{{json .Manifest.Digest}}'`, or the GHCR API it
   already calls to prove the package exists) and write `:<sha>@sha256:<digest>`. The `<sha>` tag
   stays for legibility; the digest is what runs.

5. **Pins lag HEAD by two commits.** Every tier is pinned to `1f24e39c`; HEAD is `4c7e94c9`, and the
   commit in between ("Let the agent delete a Service its definition no longer needs") is an agent
   behaviour change whose RBAC grant IS applied (`agent-rbac.yaml` already has `services: delete`).
   Not a bug — the roll follows the merge — but the RBAC widening is live ahead of the code that
   uses it, which is the ordering Important 1 should be fixed under. Fix: `deploy/pin.sh 4c7e94c9`
   on the next roll.

6. **The hostPath allow-list is owner-scoped for homes but not for volumes.**
   `deploy/k3s/workspace-admission.yaml:77` admits anything under `/wspool-prod/vol/`, while
   `:79-82` correctly pin `homes`/`homecache` to `object.metadata.labels['kloudlite.io/owner']`.
   A pod-builder bug can still mount another tenant's volume subvolume. Not exploitable from
   outside — the path comes from the agent, and volume ids are opaque — but the 2026-09-02 finding
   asked for the owner segment on all four subtrees and it landed on two.
   Fix (cost L, needs a layout change): put the owner in the volume pool path
   (`{pool}/vol/{owner}/{id}`) so the same CEL comparison works; until then, say in the file's
   comment that `vol/` is deliberately not owner-scoped and why.

7. **`api-rbac.yaml` grants `watch` and `update` on `workspaces`/`environments`; neither is used.**
   `crates/workspaces/src/api.rs` has no `.watch(`, no `watcher::`, and no `.replace(`/
   `.replace_status(` — every read is a one-shot `get`/`list`, every write a `create`/`patch`/
   `delete`. The file's own `snapshots` comment says so explicitly ("No `watch`: nothing in the API
   watches"), so the rule above it contradicts the rule below it.
   Fix (cost XS): drop `watch` and `update` from that rule.

8. **Stale Secret name in the agent-rbac header table.** `deploy/k3s/agent-rbac.yaml` says the api
   RoleBinding exists "so the API can write that namespace's git-token Secret". There is no
   `git-token` Secret: `crates/workspaces/src/k8s.rs:177` defines `USER_KEY_SECRET = "user-key"`
   and `api-rbac.yaml` restricts `resourceNames` to exactly that. Fix (cost XS): say `user-key`.

9. **`/vol-agent/` still classified in the metrics path.** `crates/core/src/metrics.rs:94` labels
   requests under `/vol-agent/` — the agent-facing volume-registry routes deleted at the workspaces
   cutover. It costs one dead `starts_with` per request and it implies a live route surface that
   does not exist. Fix (cost XS): delete the branch.

10. **`system-netpol.yaml`'s flannel `/32` list is hand-maintained.** The file says so loudly
    (`HAND-MAINTAINED: a new node's flannel.1 /32 must be added here BEFORE its agent first
    mounts`), and the failure mode is survivable — `mount_homes` runs under `timeout ... -s KILL`
    with `retry=0` (`bins/agent/src/lib.rs:64-77,154-158`), so a missing entry parks workspaces in
    `HomeNotReady` rather than wedging the agent on a `hard` mount. Left as a Minor only because
    the safety net is real. Fix (cost S, optional): add the `ip -4 addr show flannel.1` read to the
    node-join runbook checklist in `deploy/k3s/README.md` as a numbered step, not prose.

---

## Cleanup

11. **`VolExt` has four methods with zero callers anywhere, including tests.**
    `crates/workspaces/src/registry.rs:96-99` (`move_ref`, `ref_commit`, `commit`, `region`), plus
    the constants and helpers only they use: `REGION_KEY:69`, `REF_PREFIX:71`, `ref_key:75-77`, and
    the region-stamping branch inside `append_commits:122-127`. Nothing writes the volume registry
    any more — `/v1` writes `Snapshot` CRs — so the region stamp those methods exist to defend has
    no reader. Clippy does not catch this: they are `pub` trait methods in a library crate.

    What is still live: `vol_exists` and `history` (called from
    `bins/server/src/browse_api/volumes.rs:107,110`), `vol_db` (internal), and `append_commits`,
    which now has *only* test callers (`tests/browse_http.rs:1058,1112,1113`) and is what seeds the
    frozen read surface those tests cover — keep it.

    Fix (cost S, ~60 lines deleted): remove the four methods and the three items they own from
    both the trait and the impl. The trait then has four methods and one implementation; collapsing
    it further is not worth it while the orphan rule is the reason it exists (the doc comment at
    `:81-83` says so).

12. **`browse_api/volumes.rs` is frozen on purpose — do not delete it.** Its module doc rules it
    (`FROZEN (ruled: keep, don't delete)`): pre-cutover volume history has no other reader, and the
    Snapshots page would go blank for those volumes. The dead half — `volumedelete` and
    `snapshotdelete`, their `BROWSE_TAILS` entries and their `repo_of` branch — was already removed
    at `3bcdf368`, correctly and completely (`git diff 684aecaf HEAD -- bins/server` is exactly
    that removal). The retirement trigger is written down in `deploy/k3s/README.md`'s cleanup
    section. Nothing to do; recorded so the next reviewer does not re-open it.

---

## RBAC: verbs granted vs verbs used

`kloudlite-agent` (ClusterRole `kloudlite-agent` + per-namespace `kloudlite-agent-ws-secrets`).
"Used" = a call site found in `bins/agent/src/**` or `crates/workspaces/src/**`.

| Resource (group) | Granted | Used | Verdict |
|---|---|---|---|
| workspaces, environments (kloudlite.io) | get, list, watch, patch | all four (controllers, `heal_labels`, finalizers) | exact |
| volumes (kloudlite.io) | get, list, watch, create, patch, delete | all six (`ensure_child_volume`, `restore_gate`, `resolve_volume`, `collect_unreferenced_volumes`) | exact |
| ownerbindings (kloudlite.io) | get, list, watch, create | all four (`binding.rs:73,156`, `claim.rs:327`, `run.rs:224`) | exact |
| snapshots (kloudlite.io) | get, list, watch, create, patch, delete | all six (`sync.rs:91`, `snapshot.rs:92,155,190,267`, retention) | exact |
| volumereplicas (kloudlite.io) | get, list, watch, create, patch, delete | all six (`listing.rs:185`, `claim.rs:68,420`, `stop.rs:159,225`, `reap_dead_replicas`) | exact |
| */status ×6 (kloudlite.io) | patch, update | both (`patch_status` apply, `replace_status` CAS) | exact |
| */finalizers ×4 (kloudlite.io) | update | yes (+ OwnerReferencesPermissionEnforcement) | exact |
| namespaces ("") | get, create, patch | all three (`namespace_ready` get, `ensure` apply) | exact |
| limitranges ("") | create, patch | both (`binding.rs:106`, `environment.rs:374`) | exact |
| services ("") | create, patch, delete | all three (delete added `4c7e94c9`) | **exact, but DELETE is unfenced — Important 1** |
| networkpolicies (networking) | create, patch, delete | all three (attach grant both halves; `delete_ignoring_404`) | **exact, DELETE unfenced — Important 1** |
| pods ("") | get, list, watch, create, delete | all five | **exact, DELETE unfenced — Important 1** |
| statefulsets (apps) | get, list, watch, create, patch, delete | all six | **exact, DELETE unfenced — Important 1** |
| **deployments (apps)** | **get, delete** | **none — no `Deployment` type in the tree** | **DEAD — Important 2** |
| nodes ("") | get, list, watch, patch | all four (`node_roles`, rendezvous, decommission annotation) | broad `patch` taken knowingly, documented, `ponytail:`-marked |
| rolebindings (rbac) | create, patch | both (`binding.rs:121`, `environment.rs:366`) | exact; narrowed by policy 2 |
| clusterroles (rbac) | bind [2 names] | yes | exact; narrowed by policy 2 |
| secrets ("") — per-ns role | get, create | both (`ensure_ssh`) | exact; deliberately not cluster-wide |

`kloudlite-api` (ClusterRole `kloudlite-api` + per-namespace `kloudlite-api-secrets`).
"Used" = a call site in `crates/workspaces/src/api.rs`.

| Resource (group) | Granted | Used | Verdict |
|---|---|---|---|
| workspaces, environments (kloudlite.io) | get, list, **watch**, create, patch, **update**, delete | get, list, create, patch, delete | **`watch` and `update` unused — Minor 7** |
| snapshots (kloudlite.io) | get, list, create, delete | all four (`:378,1080,1258,1905,2183,2326,…`) | exact; no `/status`, deliberately |
| volumes (kloudlite.io) | get, list, delete | all three (`:1275,1289,1944,2288-2289`) | exact; no `create` (the agent authors children) |
| volumereplicas (kloudlite.io) | get, list | both (`:1107`) | exact |
| networkpolicies (networking) | delete | yes (`:993-995`, `delete_ws`'s environment-side half) | exact |
| namespaces ("") | list | yes (`:658`) | exact |
| secrets ("") — per-ns role | get, update, patch on `user-key`; create unnamed | yes (`:701,714,748-750`) | exact; the `create` split is correct and the reason is written down |

Neither ServiceAccount holds a verb it cannot justify except the two rows called out above. The
`ponytail:` note in `agent-rbac.yaml` — cluster-wide grants sharded only by the controller's own
field selector — remains the honest description of the ceiling, and the upgrade path it names
(bind the request's node identity to `status.nodeName`) is still the right one.

---

## Good — leave alone

- **`BROWSE_TAILS` ↔ router.** `route.rs:185` (24 entries, length-annotated) and the test at
  `:654` assert BOTH directions, both shapes (repo-scoped and owner-scoped), and refuse to pass
  vacuously if the scrape finds nothing. The volume-delete removal at `3bcdf368` updated the count,
  the doc comment, `repo_of`'s branch and the unit test together — this contract is doing its job.
- **`crates/app/src/lib.rs:412-470`.** The claim path's comments are the best explanation of the
  one invariant anywhere in the tree, and the residual window is marked with its exact upgrade path.
  Do not "simplify" the leader read away.
- **`system-netpol.yaml`.** The comment explains *why* the source is the node and not the agent pod
  (`nsenter -t 1 -n`), which is the non-obvious fact that makes a podSelector wrong here. Worth
  reading before touching anything NFS.
- **`harden-node.sh:57-75`.** The pod-CIDR accept was removed AND the file says what it would take
  to add one back, one port at a time.
- **`.github/workflows/`.** Every `uses:` is pinned to a 40-hex SHA with a version comment; every
  workflow declares `permissions: contents: read` at the top and widens per-job only where it must
  (`checks: write` for the audit check run, `packages: write` for the image push). Nothing to do.
- **`api-rbac.yaml`'s split `create` rule.** The comment explaining that `resourceNames` cannot
  authorize a create is the kind of thing that gets "cleaned up" back into a bug. Leave it.
- **`bins/kl/src/config.rs` `pin_host_key`.** Refuses loudly, in ssh's own words, with a named
  escape hatch. Correct.
- **`crates/registry/src/store.rs:229-262`.** The backfill's "only a COMPLETE walk may claim the
  mark" reasoning, and under-granting as the safe failure for authorization (vs the GC sweep, where
  the same skipped manifest must abort) — that asymmetry is right and is written down.
- **`tests/git_http_limits.rs:108-142`.** The negotiation-cap test asserts both directions: the
  oversized body is refused AND a kilobyte one still reaches the protocol. That second assertion is
  what stops the fix being "cap everything at zero".

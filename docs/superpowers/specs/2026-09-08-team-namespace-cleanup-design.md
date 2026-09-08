# A team's namespace goes when the team does

Status: draft for review, 2026-09-08.

## Why

`apply_binding` (`bins/agent/src/binding.rs`) creates one namespace per (person, team) pair,
`wt-{person}-{hash of team}`, for every team that person has a workspace in on that node.
Nothing ever deletes one. The comment on the create is explicit that this is deliberate — no
ownerReference, "the namespace is shared by every workspace this user owns IN THIS TEAM, and an
owner's quota ceiling must not vanish with a binding rewrite" — but the other half, removing it
once the team is gone, was never written.

Every hourly probe run makes a throwaway team, so it makes a namespace. On 2026-09-08 the region
held **101 `wt-` namespaces, every one empty**, the oldest from 2026-09-05, against **5
OwnerBindings** and **one workspace, in no team at all**. `delete_team`
(`crates/api/src/teams.rs` → `crates/pulls/src/directory/teams.rs`) refuses only on repositories
and removes nothing in the cluster.

It is not only litter. Every one carries a LimitRange and a ResourceQuota, every namespace is
watched by the pod-security admission plugin, and the churn is a plausible contributor to the
watch-cache pressure that ended the agent's `OwnerKeys` watch stream the same day.

The sibling leak — an `OwnerKeys` per dead team — is already fixed: the api's resync beat prunes
those, and 2026-09-08's `api-rbac.yaml` change gave it the `delete` verb it had always been
written to need.

## Decisions taken

| Question | Answer |
| --- | --- |
| Which tier deletes the namespace | `bins/api`'s **user role**, the workspaces side, in the existing `keys::run_beat` |
| Why not the agent | its ClusterRole says `get, create, patch` with "`get`, never `list` — the agent has no reason to enumerate namespaces it did not make", and `agent-admission.yaml` says "`namespaces` is absent on purpose — the agent holds no `namespaces: delete`, so there is nothing to fence". It is also a privileged root DaemonSet on every node; the api runs unprivileged as uid 1001. Namespace delete belongs to the smaller blast radius. |
| Why not an ownerReference | the existing reason stands: the namespace outlives any one binding, and a binding rewrite must not take an owner's quota ceiling with it |
| Why a beat and not a hook from `delete_team` | a hook fires once and is lost on a crash, and it cannot see the 101 namespaces whose teams are ALREADY gone. The beat is the record, exactly as it is for `OwnerKeys`. A hook can be added later if 5 minutes is ever too slow for a namespace. |
| What makes a namespace stale | no `Workspace` in the cluster resolves to it, it holds no pods, and it is older than one beat interval |
| Is a live team with no workspaces pruned | yes, and `apply_binding` recreates it on the next workspace. Identical to the `OwnerKeys` rule already shipped: "a projection with no workspace behind it has no pod reading its file" |
| `ws-{person}` namespaces | never pruned. One per person, not per team, and it holds that person's `user-key` Secret |

## Design

### 1. The prune (`crates/workspaces/src/api/keys.rs`, beside `project_all`)

`prune_namespaces(s: &ApiState)`, called from `run_beat` in the same pass as `project_all`:

1. List every `Workspace` (cluster-wide). A failed list **returns, pruning nothing** — the same
   keep-biased rule `project_all` already follows, and for the same reason: a lost list makes
   every namespace look stale.
2. `keep` = `crd::ws_namespace(w.spec.owner, w.spec.team)` for every workspace, plus every
   `ws-` namespace unconditionally.
3. List namespaces with `kloudlite.io/kind=workspace`. For each whose name starts with `wt-`
   and is not in `keep`:
   - skip it if `creationTimestamp` is newer than `KEYS_RESYNC_SECS` — closes the window between
     `apply_binding` creating a namespace and the workspace that needed it becoming listable;
   - skip it if it holds any pod — belt and braces against a `Workspace` whose owner label was
     lost, which is the one way step 2 could miss a live tenant;
   - otherwise delete it, and log `keys.namespace.pruned` with the name.

The `wt-` prefix is the whole selector; no new label is needed, and the 101 already-orphaned
namespaces are covered by the same rule with no migration.

### 2. RBAC (`deploy/k3s/api-rbac.yaml`)

The `kloudlite-api` ClusterRole already holds `namespaces: list`. It gains `delete`, with the
comment saying what bounds it: only `wt-` namespaces, only when no workspace resolves to them.
Nothing else in the file changes; the admin role gains nothing.

### 3. Probe

One hourly id in stage "14 · Experience", feature "Workspaces":

| id | sli | target |
| --- | --- | --- |
| `team.namespace.reaped` | The namespace of a team this run deleted is gone within two resync beats | `avail(99.9)` |

The hourly already creates a team, runs a workspace in it and deletes the team at teardown, so
the step is an assertion on what teardown already does rather than a new journey.

### 4. Failure modes

| Failure | Behaviour |
| --- | --- |
| Workspace list fails | nothing pruned, warned once, retried next beat |
| Namespace list fails | nothing pruned |
| A delete is refused | logged per namespace, the rest still run, retried next beat |
| A workspace is created into a namespace mid-prune | the age guard skips namespaces younger than one beat; a later create simply re-ensures it |
| A namespace is deleted while a pod still runs in it | cannot happen: the pod check is the second guard, and a running workspace is in `keep` anyway |

## Out of scope

Deleting a team's *workspaces* on team delete (today a team can be deleted while its members'
workspaces still exist — `delete_team` counts repositories only). That is a separate decision
about tenant data and belongs in its own spec; this one removes only namespaces nothing is
using. The `env-{id}` namespaces already carry an ownerReference and are collected by Kubernetes.

## Files

`crates/workspaces/src/api/keys.rs` (the prune and its tests), `deploy/k3s/api-rbac.yaml`
(`delete`), `bins/slo/src/stages/experience_teams.rs` (the step),
`crates/workspaces/src/slo/catalogue.rs` + `deploy/slo.md` +
`web/apps/web/src/lib/fixtures/superadmin.ts` (the row), `CLAUDE.md` (one sentence).

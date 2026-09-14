# A person's space follows one environment

Status: design, 2026-09-14. Replaces per-workspace attach (`Workspace.spec.attachedEnvironment`,
`Bench.spec.attachedEnvironment`).

## The request and the rulings

"All my workspaces connected to the same environment." Binding rulings:

1. **Scope is a person's SPACE in a team**: one environment per (person, team), plus one for the
   person's personal owner. Teammates choose independently.
2. **It replaces per-workspace attach** everywhere — API, web, desktop, `kl_*` tools. Every pod of
   the space follows the choice, existing and future, running ones live, without a restart.
3. **Reach is the whole space**: every pod the platform runs for that person in that team —
   workspaces, the bench, and any kind added later — resolves the environment's services by bare
   name through ONE mechanism, not per-kind wiring.

## The observation that makes this small

A space already has a name: `crd::ws_namespace(owner, team)` is one Kubernetes namespace per
(person, team) pair (`ws-{handle}` personal, `wt-{handle}-{tail}` team), and `Workspace` and
`Bench` both live in it with `spec.owner` = the person's handle and `spec.team` = the team. So
"the person's space" = that namespace, and "every pod of the space, including future kinds" =
every pod in that namespace. The per-pod selectors in `k8s::attach_egress`/`attach_ingress`
existed only because the grant had to stop at one workspace (the comment says so: "a
namespace-wide grant would open every workspace they own"). That is now exactly the wanted
behaviour, so the selector widens to the namespace and the kind-specific wiring disappears.

The builder is NOT in the space: `bld-{slug}` is one per TEAM in its own `env-` namespace, shared
by every member. It must not follow one member's choice (a teammate's build would resolve
someone else's services), so it stays out, deliberately.

## Storage: two approaches

**A. New CR `SpaceEnvironment`** (recommended). `kloudlite.io/v1alpha1`, cluster-scoped, named
by the space's namespace name (`ws_namespace(owner, team)` — already unique, DNS-safe,
deterministic on both tiers, and ≤ 55 chars).

```yaml
spec:  { owner: alice, team: acme, environment: env-3f2a }   # written only by /v1
labels: kloudlite.io/owner=alice, kloudlite.io/team=acme, kloudlite.io/environment=env-3f2a
```

No status: the observed fact stays where readers already look, the `Attached` condition on each
Workspace/Bench (message = env id). Absent object = no environment. Labels are views of spec
(`heal_labels` rule); `environment` label is what `delete_env` selects on.

**B. Field on `OwnerBinding`.** Rejected: a binding is per `{region, owner}` (`binding_name`),
not per (person, team) — the key does not fit without a reshaping migration of an object every
node reconciles for namespaces and quota; every choice change would re-run that whole reconcile
on every node; and it would put a person-set value on an object whose spec is otherwise
infrastructure.

A wins on: exact key, tiny watch, trivially RBAC-separable (agent `get/list/watch` only), deletable
on its own when a member leaves.

## Authorization (`/v1`, `bins/api` user role)

- The person is always the CALLER. There is no way to set another person's choice; the body
  carries no `owner` (400, same rule as `/v1/keys`).
- `team` is the caller's handle (personal) or a team the caller is a member of
  (`may_act_on(team)`); otherwise 404.
- The environment must exist, pass `visible_env` (the builder 404s), and have
  `spec.owner == team` (personal: `== handle`). Environments are team-owned; a person may not
  point a team space at another owner's environment even if they can see it elsewhere.
- Region is not checked at write time — a space spans regions, an environment does not. A pod in
  another region reports `Attached=False/RegionMismatch` exactly as today and resolves nothing.

Lifecycle:

| Event | Effect |
|---|---|
| Environment deleted | `delete_env` deletes every `SpaceEnvironment` labelled `environment={id}` (replaces today's two clear-the-field loops). Agent treats a missing env as none regardless. |
| Environment stopped | Choice stays; Services keep their names, dials are refused until it starts. No change to the space. |
| Member removed from team | The api's keys resync beat (`KEYS_RESYNC_SECS`, already running on membership change) deletes `SpaceEnvironment`s whose `owner` is no longer a member of `team`. Worst case one beat of stale grant, same bound as keys. |
| Person switches env | PUT overwrites spec; running pods move within one reconcile. |

## Agent fan-out

- **One new reflector**: an unfiltered cluster-wide `SpaceEnvironment` cache in `Ctx`, opened in
  `controller/run.rs` beside `Ctx::workspaces()`, read through `Ctx::spaces()` which returns
  `None` until the first list (`controller::store_ready`). Never a GET per pass.
- **Triggers**: the Workspace and Bench controllers each gain a `.watches` on `SpaceEnvironment`
  whose mapper returns the objects in that namespace from their own stores (same shape as the
  `spec.attachedEnvironment` mapper at `run.rs:398`, which is deleted).
- **The one resolver** `space_environment(ctx, owner, team) -> Known(Option<Environment>) | Unknown`
  (new, `controller/space.rs`), used by the workspace reconciler, the bench reconciler, the
  intercept decision and any future pod kind. `Unknown` (cache not listed) means: do NOT rewrite
  resolv.conf, do NOT delete a policy, requeue — an unlisted cache read as "no environment" would
  strip DNS from every pod in the region on every agent restart.
- **Per pass, for a pod in namespace `ns`** (replacing `attach_grants` in `bench.rs` and the
  block at `workspace/mod.rs:345`):
  1. resolve the space's environment (missing / cross-region → none + condition reason);
  2. `write_resolv_conf(pool, pod_id, ns, env_ns)` — unchanged, per pod, in place;
  3. ensure the two namespace-level policies (below), or delete the egress half when none;
  4. delete the legacy `attach-{pod_id}` policy in `ns` and in the env named by the old
     `Attached` message (the collection path that exists today, kept one release);
  5. write `Attached` as today.

  Steps 3–4 are idempotent SSA, so two nodes hosting pods of the same space converge on the same
  objects; `ensure` already skips an unchanged write (no echo storm).
- **Future pod kinds** get it for free for egress (namespace-wide policy). DNS needs the mount:
  `k8s::space_resolv_mount(pool, pod_id)` becomes the only way a pod in a `ws_namespace` gets
  `/etc/resolv.conf`, and a `k8s/tests` case asserts every pod builder targeting a space namespace
  carries it. That test, not convention, is what "anything added later" rests on.

### NetworkPolicy shape (per space, not per pod)

| Name | Namespace | Selects | Owner |
|---|---|---|---|
| `space-env` | space `ns` | `podSelector: {}`, egress to `namespaceSelector name=env_ns` | the `SpaceEnvironment` |
| `space-{ns}` | `env_ns` | `podSelector: {}`, ingress from `namespaceSelector name=ns` | the `SpaceEnvironment` |

A cluster-scoped owner may own namespaced objects, so both halves are garbage-collected when the
choice is deleted — better than today, where the env-side half needed `delete_ws` to remove it.
On a SWITCH the old env's `space-{ns}` would linger (same owner): the Environment reconciler's
existing `prune_attach_grants` is extended to drop any `space-*` ingress in its namespace that
`Ctx::spaces()` does not point at it (Unknown → keep). A lingering half is harmless meanwhile: its
egress counterpart is gone.

### resolv.conf

Mechanism unchanged: `{pool}/attach/{pod_id}/resolv.conf`, `hostPath type: File`, written in
place, never renamed, env namespace first in `search`. Keeping the per-pod path (rather than one
file per namespace) is what makes this live: the mount path of a running pod cannot change.
Janitor `janitor_sweep_attach` is unchanged.

## Intercepts: what "attached" means now

A workspace is attached to environment E iff `space_environment(w.owner, w.team)` is E and the
workspace is in E's region. Both checks move to that one function:

- `/v1` `validate_intercept`: reads the `SpaceEnvironment` by name (a single GET is fine in the
  api tier) instead of `w.spec.attachedEnvironment`; 409 sentence becomes "that workspace's space
  does not use this environment; choose it first".
- agent `decide_intercept` (`environment/intercept.rs:179`): `Unknown` → `Keep`; a different
  choice → `off("WorkspaceDetached", …)`, still releases after grace, wish kept.

Consequence worth stating: switching a space to another environment releases every intercept its
workspaces hold in the old one (wishes stay, resume if switched back).

## Migration

1. `attachedEnvironment` stays in both CRD structs and in `SnapshotState::Workspace`, parseable,
   documented as retired; nothing writes it. Clone/restore ignore it (a frozen value is not
   carried).
2. The api's user role runs a one-shot at boot and on the resync beat until nothing remains:
   for each Workspace/Bench with the field set and no `SpaceEnvironment` for its space, create
   one if the environment still passes the authorization rules above. Conflicting values in one
   space: the most recently updated object wins, the rest are logged `space.migrate.conflict`.
   It then clears the field (merge patch) so the beat terminates.
3. Until step 2 has run, the agent resolver falls back to the pod object's own
   `attachedEnvironment` only when the space cache is KNOWN and has no entry — so a rolled agent
   never drops a live attach during the window between agent and api rolls.
4. A later release removes the fallback, the field, `ATTACHED_ENV_LABEL`, `heal_attached_label`
   and the `attach-{id}` cleanup.

## API surface

| Route | Body / answer |
|---|---|
| `GET /v1/me/environments` | `[{team, environment, region}]` for the caller (personal entry has `team` = handle) |
| `PUT /v1/me/environments/{team}` | `{environment}` → 200; 404 team/env; 409 not the team's env |
| `DELETE /v1/me/environments/{team}` | 204, idempotent |
| `GET /v1/workspaces/{id}`, `/v1/bench` | `attachedEnvironment` field keeps its name, now computed from the space + `Attached` |
| `POST /v1/workspaces/{id}/attach|detach`, `/v1/bench/attach|detach` | 410 with "an environment is chosen per team now: PUT /v1/me/environments/{team}" for one release, then removed |

Every write logs `http.write`. RBAC: `kloudlite-api` SA gets `create/patch/delete` on
`spaceenvironments`; `kloudlite-admin` read; agent `get/list/watch`; the admission policy needs no
change (agent has no write verb). Ingress: add `me` to the allow-list at
`deploy/kloudlite-web.yaml:252` — the harness talks to the api directly.

## UI and tools

- **Web**: remove the attach choice from `components/repo/open-in-workspace.tsx` and the
  create flow in `workspaces/actions.ts` (and `api.attachWorkspace` in `lib/api/workspaces.ts`).
  Add one control on the team's environments list and environment detail: "Use for my
  workspaces" / "In use by your workspaces" / "Stop using". Workspace detail shows the space's
  environment read-only with a link. `intercept-control.tsx` copy: "attached" → "in a space that
  uses this environment". Add `lib/api/me.ts` (`myEnvironments`, `setMyEnvironment`,
  `clearMyEnvironment`).
- **Desktop** (`harness/src/renderer/components/EnvironmentDock.tsx`, `EnvironmentPage.tsx`):
  the same three states per team; `model.ts`'s embedded mapper snippet updated.
- **Tools** (`harness/pi/kloudlite.ts`, `catalog.ts`): delete `kl_workspace_attach`; add
  `kl_my_environment` (GET), `kl_my_environment_set {team, environment}`,
  `kl_my_environment_clear {team}`. `kl_workspace`'s summary stays accurate via the computed field.

## SLO probe (`bins/slo/src/stages/environment.rs`, `deploy/slo.md`, `slo/catalogue.rs`)

- `env.attach` / `env.detach`: same ids and targets, same in-pod lookup, driven by
  PUT/DELETE `/v1/me/environments/{team}` instead of the workspace routes.
- New `env.space.live` (fast): a SECOND workspace, already running before the PUT, resolves the
  service within the ceiling — the "no restart, all workspaces" promise.
- New `env.space.bench` (hourly): the probe owner's bench resolves the service by bare name.
- `env.intercept.refused`: the "unattached" case is a workspace whose space uses no environment.
- `env.attach.pair` retired (a workspace delete no longer owns an env-side policy); replaced by
  `env.space.cleared` (hourly): DELETE removes `space-{ns}` from the env namespace within 30 s.
- Teardown additionally deletes the probe owners' `SpaceEnvironment`s (named deterministically).
- Held equal by the existing slo.md ↔ catalogue test.

## Failure modes

| Failure | Behaviour |
|---|---|
| Space cache not listed (agent restart) | Unknown → no rewrite, no delete, intercepts Keep, requeue |
| Environment gone / cross-region | `Attached=False/EnvironmentNotFound|RegionMismatch`, resolv.conf without env, egress removed |
| Pod predates the resolv.conf mount | Existing `pod_carries_the_attach_mount` refusal stays ("stop and start once") |
| resolv.conf truncate window | One failed lookup, resolver retries (accepted today) |
| api migration crashes mid-run | Field not yet cleared → re-run on next beat; agent fallback keeps DNS |
| Member removed | Stale grant ≤ one keys beat; bench is already `ReadOnly` |
| Two nodes host pods of one space | Idempotent SSA on identical policies; no contention |
| No network policy engine (AKS today) | DNS works, policies are inert — same as today's attach |

## Rollout order

1. CRD `SpaceEnvironment` + RBAC (`deploy/k3s/agent-rbac.yaml`, `api-rbac.yaml`,
   `deploy/kloudlite.yaml`), applied by hand on k3s per its README.
2. Agent: space cache, resolver with field fallback, namespace policies, legacy cleanup,
   intercept decision. Old api still writes the field → fallback keeps it working.
3. Api: `/v1/me/environments*`, migration beat, 410s, `delete_env` + member-removal cleanup,
   `validate_intercept`; ingress `me`.
4. Web, desktop, pi tools (the 410s make a stale client fail loudly, not silently).
5. SLO probe image + slo.md/catalogue.
6. Next release: drop the fallback, the field, the label and legacy cleanup.

## Tests

- `crd`: `SpaceEnvironment` naming = `ws_namespace`; old objects with `attachedEnvironment` still
  parse.
- `k8s`: `space_egress`/`space_ingress` select the namespace, AND-free single `from` element;
  every space-namespace pod builder carries the resolv mount.
- `bins/agent/tests/reconcile/attachment.rs` rewritten: two workspaces + a bench in one space get
  env DNS from one choice; teammate's space unaffected; switch rewrites resolv.conf in place and
  prunes the old env's ingress; unknown cache touches nothing; legacy `attach-{id}` removed;
  field fallback only when the cache is known and empty.
- intercept: space pointing elsewhere → `WorkspaceDetached`; unknown → Keep.
- `crates/workspaces/tests/api_*`: PUT authz matrix (non-member 404, other owner's env 409,
  builder 404, body `owner` 400); delete_env deletes by label; member removal beat; migration
  conflict and idempotence; attach routes 410.
- web `bun test` for `lib/api/me.ts`; SLO catalogue equality test.

## Open questions

1. Personal-space environments: environments are "usually" team-owned — may a personal space
   choose only the person's own environments (as specified), or also a team's?
2. Migration conflict (two workspaces of one space attached to different environments): is
   "most recently updated wins" acceptable, or should the space be left unset?
3. The 410 window for the old attach routes — one release, or remove immediately since every
   client ships in the same rollout?

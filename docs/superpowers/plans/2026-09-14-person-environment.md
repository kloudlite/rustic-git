# Plan: a person's space follows one environment

Spec: `docs/superpowers/specs/2026-09-14-person-environment-design.md`. Tasks follow the spec's
rollout order; one commit per task, pushed to `platform` after each.

## 1. CRD and RBAC
- `crates/workspaces/src/crd/space.rs`: `SpaceEnvironment` (cluster-scoped, `{owner, team,
  environment}`, status subresource kept empty so the crd_yaml invariant holds), `space_name`
  = `ws_namespace`, `TEAM_LABEL`/`ENVIRONMENT_LABEL`; retire-doc `attachedEnvironment` on
  `Workspace`/`Bench`/`SnapshotState`.
- Regenerate `deploy/k3s/crds.yaml` (`CRD_REGEN=1 … --test crd_yaml`).
- `deploy/k3s/agent-rbac.yaml` (table row + rule: get/list/watch), `deploy/k3s/api-rbac.yaml`
  (user: get/list/create/patch/delete; admin: get/list). Admission policy unchanged.
- Tests: naming = `ws_namespace`; old objects with `attachedEnvironment` still parse.

## 2. Agent
- `crates/workspaces/src/k8s/policies.rs`: `space_egress` (`space-env` in the space ns) and
  `space_ingress` (`space-{ns}` in env ns), namespace selectors only; tests.
- `bins/agent/src/controller/mod.rs`: `space_store`/writer, `Ctx::spaces()` (None until listed),
  `remember_spaces` for tests. `run.rs`: third cache watch; Workspace and Bench controllers watch
  `SpaceEnvironment` mapping to objects of that namespace; Environment controller watches it too
  (all in store) and drops the `spec.attachedEnvironment` mapper.
- `bins/agent/src/controller/space.rs`: `space_environment` resolver (`Unknown` / `Known`), with
  the field fallback only when known and absent; `converge_space` shared by workspace and bench:
  resolv.conf, namespace policies, legacy `attach-{id}` cleanup, `Attached` condition.
- `environment/mod.rs`: prune also drops `space-*` ingress not pointed at this env (Unknown keeps).
- `environment/intercept.rs`: attached = space resolver; Unknown → Keep.
- Tests: `bins/agent/tests/reconcile/attachment.rs` rewritten per the spec's list.

## 3. Api
- `crates/workspaces/src/api/me.rs`: `GET /v1/me/environments`, `PUT|DELETE
  /v1/me/environments/{team}`; authz (caller only, body `owner` 400, team membership 404, env
  visible + `spec.owner == team` else 404/409).
- 410 on the four attach/detach routes; `delete_env` deletes `SpaceEnvironment`s by label;
  `validate_intercept` reads the space; `WsDoc.attachedEnvironment` computed from the space.
- `crates/workspaces/src/api/spaces.rs` beat (on the keys beat): migration (most recently updated
  wins, `space.migrate.conflict`, clears field) and member-removal prune.
- `deploy/kloudlite-web.yaml` ingress: add `me`.
- Tests: `crates/workspaces/tests/api_spaces.rs`; old attach tests replaced.

## 4. Clients
- Web: `lib/api/me.ts` (+ test), environment list/detail control, remove attach from create flow
  and `open-in-workspace.tsx`, intercept copy, workspace detail read-only line.
- `harness/pi/kloudlite.ts`/`catalog.ts`: drop `kl_workspace_attach|detach`, add
  `kl_my_environment*`. Desktop renderer is out of scope for this pass (another agent owns it).

## 5. SLO probe
- `bins/slo/src/stages/environment.rs`: attach/detach via `/v1/me/environments/{team}`;
  `env.space.live`, `env.space.bench`, `env.space.cleared`; retire `env.attach.pair`;
  teardown deletes the probe owners' spaces. `deploy/slo.md` ↔ `slo/catalogue.rs`.

## 6. Next release (not in this branch)
Drop the fallback, the field, `ATTACHED_ENV_LABEL`, `heal_attached_label`, legacy cleanup, 410s.

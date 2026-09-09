# One kind of node

Status: draft for review, 2026-09-09. Discarded in practice long ago; this removes it from the
code so nothing can act on it again.

## Why

The agent still carries a node-role concept from the first capacity sheet: a node was either a
"session" node (workspaces) or an "env" node (environments), advertised by the labels
`kloudlite.io/session=true` and `kloudlite.io/env=true`, read once at startup into `Ctx.roles`,
and consulted in three places:

| Where | What it does |
| --- | --- |
| `bins/agent/src/controller/run.rs` | starts the Workspace claim watch only on a `session` node and the Environment claim watch only on an `env` node |
| `crates/workspaces/src/k8s.rs::placement` | the pod's `nodeSelector` demands `kloudlite.io/{role}=true` beside the hostname, and tolerates a `kloudlite.io/{role}` taint |
| `bins/agent/src/claim.rs::admissible_pct` | an env-only node is packed to 80 % of allocatable, anything else to 100 % |

Every node on every region carries BOTH labels, none carries the taint, and the decision to run
one kind of node was taken well before today. What is left is a distinction nothing uses, that
still costs: a node missing one label silently claims nothing of that kind (`node.labels.missing`
is a warning nobody reads), a `ponytail:` note in `run.rs` calls a dual-role node "the dev
exception" when it is every node, a plan written today pinned a spike to "an env node", and the
capacity rule in `admissible_pct` has a branch no node can reach.

## Decisions taken

| Question | Answer |
| --- | --- |
| The label that means "this node hosts tenant workloads" | `kloudlite.io/pool=true`, which already exists on every node and already means exactly that (the btrfs pool is mounted). No new label. |
| Claim watches | Both, on every node with the pool, unconditionally. `Ctx.roles` goes; `node_roles` becomes `node_has_pool`, and a node WITHOUT the pool label runs no claim watch and warns — the same fail-safe as today, one condition instead of two. |
| Placement | `placement(spec, node)`: selector `kloudlite.io/pool=true` + hostname; toleration on `kloudlite.io/pool`. The `role` argument goes. |
| Capacity | 100 % of the guarantee on every node — the rule the session branch had, which is the rule every node has in fact been under. The 80 % "env packing" was the sheet's steady-state target for a kind of node that does not exist; `admissible_pct` becomes a constant and the comment says why. |
| The `kloudlite.io/session` and `kloudlite.io/env` labels | Left on the nodes; nothing reads them after this. The README stops telling anyone to set them. Removing them from live nodes is one `kubectl label node … -` per region, done at rollout, recorded in the README's history. |
| The old `kloudlite-git.io/*` label copies | Untouched; a rename's leftovers, out of scope. |

## Design

1. `bins/agent/src/lib.rs`: `node_roles` → `async fn node_has_pool(client, node) -> bool`, reading `kloudlite.io/pool`. Unreadable Node or missing label → `false` with the existing `node.labels.missing` warning (reason `no-pool-label`). `Ctx::new` takes `has_pool: bool` in place of `roles`.
2. `bins/agent/src/controller/run.rs`: `claim_ws` and `claim_env` are both `ctx.has_pool.then(|| …)`. The `ponytail:` note about a node carrying both labels is deleted — the claim is atomic, both watches on every node is the intended shape, and there is nothing left to upgrade.
3. `crates/workspaces/src/k8s.rs`: `placement(spec: &mut PodSpec, node: &str)`; the two callers (`workspace_pod`, `service_statefulset`) drop their role string. The comment about "one label key per role" goes with the roles; the one that stays says why the pool label is the selector (the data is on this node's pool, and a node without one cannot host it).
4. `bins/agent/src/claim.rs`: `admissible_pct` → `const ADMISSIBLE_PCT: u64 = 100`, with the sheet's reasoning kept in one sentence: the guarantee is not oversubscribed, anywhere.
5. `deploy/k3s/README.md`: the two `kubectl label node` lines become one (`kloudlite.io/pool=true`), and the rollout note records the label removal. `docs/capacity-model.md` gets a one-line note that node kinds are gone and the 100 % rule is fleet-wide.
6. Tests: every fixture that set `kloudlite.io/session` sets `kloudlite.io/pool`; `admissible_pct`'s two tests become one; a new test that `placement` selects on `pool` and nothing else besides the hostname; a new test that a node without the pool label starts no claim watch (the existing shape of the boot test).

## Failure modes

| Failure | Behaviour |
| --- | --- |
| A node with pods but no `pool` label after the roll | Impossible on the fleet (every node has it); on a fresh install the agent warns `no-pool-label` and claims nothing, as it does today for a missing role label |
| A stored pod with the old selector | Pods are re-rendered on every reconcile with a forced apply; a running pod keeps its old selector, which is still satisfied (the labels stay on the nodes), and picks up the new one on its next recreate |

## Out of scope

Removing the `kloudlite-git.io/*` labels. Renaming the nodes. Any change to the capacity numbers
beyond dropping the unreachable branch.

## Files

`bins/agent/src/lib.rs`, `bins/agent/src/controller/{mod.rs,run.rs}`, `bins/agent/src/claim.rs`,
`crates/workspaces/src/k8s.rs`, `bins/agent/tests/reconcile.rs`, `deploy/k3s/README.md`,
`docs/capacity-model.md`, `CLAUDE.md` (the sentence naming role labels, if any).

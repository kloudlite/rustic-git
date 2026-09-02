# Codebase review remediation — plan index

Spec: `docs/superpowers/reviews/2026-09-02-codebase-review.md` (details under
`docs/superpowers/reviews/2026-09-02-details/`). Five independent plans, each executable on its own
worktree with `superpowers:subagent-driven-development`. Run them in this order; the arrows are the
only cross-plan dependencies.

| order | plan | tasks | what it closes | notes |
|---|---|---|---|---|
| 1 | `2026-09-02-review-deploy-security.md` | 8 | High #1, #2, #3, #7; deploy Mediums and Lows | Manifests apply straight to the k3s cluster; the runtimeClass validation is deny-on-apply, so the plan checks every running tenant pod first. `kl` host-key pinning is a `kl` release. |
| 2 | `2026-09-02-review-server-core.md` | 9 | High #4; server/core Mediums and Lows | Existence-gated claims (not a token bucket), metrics on their own listener, one `ssh_fingerprint` in `core` behind a feature. |
| 3 | `2026-09-02-review-agent.md` | 23 | High #5, #6; agent Mediums, test gaps, shared per-beat listing, controller split | **Rollout order matters**: the CRD `selectableFields` change (`deploy/k3s/crds.yaml`) must be applied before the agent that uses it rolls, or every stop's flush gate 400s. The controller split is pure moves, one file per task. |
| 4 | `2026-09-02-review-registry.md` | 11 | registry/pulls Mediums and Lows | `max_layer` default drops to 5 GiB; mergeability verdicts stamped with the tips they were computed from. |
| 5 | `2026-09-02-review-web.md` | 6 | High #8; web Mediums and Lows | `createdAt` fix ships with the api it reads; roll web after the api tier. |

Deferred across plans, each with its reason recorded in the plan's Self-review: gateway jti replay
(needs Redis; TTL is the mitigation), `revoke_tokens_for` index (nothing runs it on a schedule),
merge-cache lock scope (test would be a wall-clock flake), the worktree-materialize duplication
(behaviour-carrying, after the split), `live_state` removal (web-owned), README round trip and
unbounded web lists (need API changes), `env.example.sh` (a template, not a defect).

Two review corrections the plans carry: the hostPath owner check can only cover `homes/` and
`homecache/` (volume and attach paths are keyed by id, not owner); retention selects on the owner
label, because `VOLUME_LABEL` is stamped on Snapshots, not on parents.

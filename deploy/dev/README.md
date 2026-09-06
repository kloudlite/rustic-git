# Working from the dev pod

Nothing is built or tested on the laptop. The checkout, the cargo target and the registry cache
live on the `dev-work` disk of the `dev` pod in namespace `kloudlite`, on the tainted `builder`
node of the main AKS cluster (`deploy/dev/builder.yaml`). The pod's checkout is fed by **git
only**: edit and commit anywhere (the pod itself, via `exec.sh`, or the laptop), push, and the pod
pulls. There is no rsync, so what runs in the cluster is always a pushed commit.

| want | run |
| --- | --- |
| bring the pod to origin/master and build | `deploy/dev/sync.sh` (every binary) or `deploy/dev/sync.sh --slo` (probe + `kl` only) |
| tests, clippy | `deploy/dev/test.sh -p <crate> --lib …` (pulls first); clippy: `deploy/dev/exec.sh cargo clippy --workspace --all-targets -- -D warnings` |
| a shell in the checkout | `deploy/dev/exec.sh` |
| run a probe suite | `deploy/dev/slo.sh fast\|hourly\|weekly\|monthly` — the CronJob's tenant, key and budget, no image |
| serve a tier from the pod | build it, run the binary in the pod with that tier's env, then `kubectl -n kloudlite patch svc <tier>` to select `app: dev` (label the pod) — and put the selector back |

## Git: the pod pulls, and can push

`/work/src` is a normal clone of `kloudlite/rustic-git`. `sync.sh`/`test.sh` fast-forward it to
`origin/master`; an edit made in the pod is committed and pushed from the pod (`exec.sh`, then
`git commit && git push`), and the laptop catches up with `git pull`. Uncommitted edits in the pod
block the fast-forward — commit or stash them first.

One-time, done by a person because it is a login:

```sh
deploy/dev/exec.sh                                   # a shell in /work/src
gh auth login --hostname github.com --git-protocol https --web   # or --device
gh auth setup-git                                    # git push uses gh's token from now on
git config --global user.name  "Karthik Th"
git config --global user.email "karthik@kloudlite.io"
```

`HOME` is `/work/home` on the disk, so the login, the identity and `safe.directory` survive a pod
restart. Done 2026-09-06 as `karthik1729`. The commit-msg hook is not in the pod — keep the same
rule by hand: no tool attribution in messages.

Rules that keep this honest:

- **Never roll over a probe run.** `deploy/roll.sh` waits for every SLO Job; before the hand-run
  k3s agent apply use `deploy/roll.sh --wait-only`.
- **Schedules stay suspended while iterating** (`kubectl -n kloudlite patch cronjob kloudlite-slo-fast
  -p '{"spec":{"suspend":true}}'`, same for hourly). `deploy/roll.sh` re-enables them from the
  manifest; re-suspend after a roll until every suite is green, then switch them back on.
- **Fail fast.** Run a suite under a watcher that deletes the Job on the first failed step, then
  close that run's `running` row (`PUT /admin/slo/runs/{id}` with its own steps and
  `state: failed`) or the next run of the suite yields to the ghost for ten minutes; delete the
  killed run's leftover workspaces so they do not crowd a node.
- **Fixed means it does not repeat**: a change is "changed, unverified" until the failing step has
  passed on the fleet on the build carrying it.
- **Agent changes still need an image**: the k3s agents run from `ghcr.io`, so a change under
  `bins/agent` or `crates/workspaces` used by the agent goes through `deploy/k3s/dev-push.sh` (build
  VM) or CI; the pod covers the probe and the AKS tiers.
- The pod runs as root and `rsync -a` keeps the laptop's uid on the files, so git in the pod
  needs `git config --global --add safe.directory /work/src` once per pod restart (the pod's
  own root filesystem is not on the disk).

Cost: one Standard_D16s_v5 (about $0.8/hour). Delete the pool when the campaign is over:
`az aks nodepool delete -g kolomi-rg --cluster-name kolomi-cluster -n builder`.

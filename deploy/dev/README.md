# Working from the dev pod

**Edit here, not on the laptop.** Browse and change files in `/work/src` (a shell via `exec.sh`, or
a script piped into `kubectl exec -i`), then commit and push from the pod. The laptop's clone is a
mirror that only pulls. Nothing is built or tested on the laptop. The checkout, the cargo target and the registry cache
live on the `dev-work` disk of the `dev` pod in namespace `kloudlite`, on the tainted `builder`
node of the main AKS cluster (`deploy/dev/builder.yaml`). The pod's checkout is fed by **git
only**: edit and commit anywhere (the pod itself, via `exec.sh`, or the laptop), push, and the pod
pulls. There is no rsync, so what runs in the cluster is always a pushed commit.

| want | run |
| --- | --- |
| bring the pod to origin/master and build | `deploy/dev/sync.sh` (every binary) or `deploy/dev/sync.sh --slo` (probe + `kl` only) |
| tests, clippy | `deploy/dev/test.sh -p <crate> --lib …` (pulls first); clippy: `deploy/dev/exec.sh cargo clippy --workspace --all-targets -- -D warnings` |
| a shell in the checkout | `deploy/dev/exec.sh` |
| run a probe suite, fail fast | `deploy/dev/run-suite.sh weekly` (kills on the first failed step, closes the row); `--no-fail-fast` to let it finish; `--attach /work/runs/<log>` to watch one already running |
| watch anything live | `deploy/dev/attach.sh` (`pm2 logs`, every process), `attach.sh weekly` / `attach.sh watch-weekly` / `attach.sh build` / `attach.sh test` (one), `attach.sh monit` (dashboard), `attach.sh list`; `attach.sh shell` is a tmux shell in the pod. Every long process — a suite, its fail-fast watcher, a build, a test run — is a pm2 process in the pod; nothing runs on the laptop |
| watch a run live, non-interactively | `deploy/dev/tail.sh` (one line per step), `kubectl -n kloudlite logs -f deploy/dev` or k9s (raw JSON lines), HyperDX (search `k8s.pod.name:dev-*`, filter `ok:false` or an `slo_id`) — the run also streams to the pod's stdout; the console's SLO page shows it stage by stage |
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

## Images from the pod (`ship.sh`)

`deploy/dev/ship.sh` is CI in the pod: clippy + tests (CI's commands), `cargo build --release`,
then the four images built by the `buildkitd` sidecar from the real Dockerfile and pushed as
`ghcr.io/kloudlite/<image>:<full sha>` (+ `latest`) — the same tags CI writes, so `deploy/pin.sh
<sha>` and `deploy/roll.sh` follow unchanged. It refuses a dirty tree or a HEAD that is not
`origin/master`: the tag must mean the code GitHub has. CI still runs on master as the safety
net; nothing waits on it. One-time: in the pod, `gh auth refresh -s write:packages` then
`crane auth login ghcr.io -u <github user> -p "$(gh auth token)"` (buildctl reads the same
`$DOCKER_CONFIG`). Build cache lives in `/work/buildkit`.

## Verifying a pinned image (`run-job.sh`)

`deploy/dev/run-job.sh <suite>` runs the suite as a Job from its pinned CronJob (the real image,
the real env) and follows it. On the first failed step it force-kills the probe pod — a plain
`delete job` leaves the pod running through its grace period, and it went on to run a drain drill
and label a node — closes the run's row, sweeps its objects (`pod/close-run.py`) and removes any
decommission label a drill left behind. pm2 runs (`run-suite.sh`) are for code in the pod; a Job is
the verdict on what is pinned.

## Web changes

Bun and Node 20 live on the disk too (`/work/bun`, `/work/node`, on the pod's PATH; install once
with `curl -fsSL https://bun.sh/install | BUN_INSTALL=/work/bun bash` and the Node 20 tarball into
`/work/node`). `cd /work/src/web && bun install && bun run typecheck && bun run lint && bun run
test && bun run build` is the gate `web.yml` runs; the web image itself is CI's (`web.yml` on a
push touching `web/**`), then `deploy/pin.sh <sha> <web-sha>` and `deploy/roll.sh`.

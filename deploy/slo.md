# SLOs

One synthetic user, `kloudlite-git-slo`, walks this whole table as a Kubernetes `CronJob` — fast
every 5 min, weekly and monthly add the heavy checks and the resilience drills — and reports each
step while it runs to the admin process, which computes 30-day attainment, error budget and burn
rate per row. The catalogue lives in Rust (`crates/workspaces/src/slo/catalogue.rs`); this file is
its human twin, held equal by `the_catalogue_matches_deploy_slo_md`, exactly as `deploy/alerts.md`
is held to `history::alerts`. A row here with no `id` (Suite `manual`) is not probed — only a
person can do it (an email link, a passkey registration, a restore drill) — and never appears in
`CATALOGUE`.

A latency SLO ("Target" has a `≤ N ms") is "good" only when the step both succeeded and stayed
under that ceiling; an availability-only SLO ("Target" is a bare percentage) is good whenever the
step succeeded. `SloBurn` and `SloProbeMissing` (`deploy/alerts.md`) evaluate every row below.

| id | Feature | SLI | Target | Suite | Journey step |
| --- | --- | --- | --- | --- | --- |
| `id.signin` | Identity | Sign-in over HTTP succeeds | 99.9 % | fast | 1 · identity |
| `id.token.mint` | Identity | Minting a user JWT succeeds | 99.9 % | fast | 1 · identity |
| `id.key.usable` | Identity | A freshly minted platform SSH key is usable | 99.9 % ≤ 5000 ms | fast | 1 · identity |
| `id.cli.flow` | Identity | The kl CLI's login-to-command flow completes | 99.9 % ≤ 15000 ms | fast | 1 · identity |
| `id.jwt.tiers` | Identity | A JWT is honoured across every tier | 99.9 % | fast | 1 · identity |
| `git.push.ok` | Git hosting | Push of one commit over HTTP succeeds | 99.9 % | fast | 2 · git |
| `git.push.p95` | Git hosting | Push of one commit over HTTP completes | 95 % ≤ 3000 ms | fast | 2 · git |
| `git.clone.ok` | Git hosting | Clone over HTTP succeeds | 99.9 % | fast | 2 · git |
| `git.clone.p95` | Git hosting | Clone over HTTP completes | 95 % ≤ 2000 ms | fast | 2 · git |
| `ssh.clone.ok` | Git hosting | Clone over SSH succeeds | 99.9 % | fast | 2 · git |
| `ssh.hostkey` | Git hosting | The SSH host key served matches the pinned fingerprint | 100 % | fast | 2 · git |
| `ssh.unregistered.refused` | Git hosting | SSH from an unregistered key is refused | 99.9 % | fast | 2 · git |
| `browse.p95` | Git hosting | The Browse API renders a repo page | 95 % ≤ 500 ms | fast | 2 · git |
| `browse.commit.visible` | Git hosting | A pushed commit becomes visible in Browse | 99.9 % ≤ 5000 ms | fast | 2 · git |
| `web.repo.page` | Git hosting | The web app's repo page loads | 95 % ≤ 1500 ms | fast | 2 · git |
| `pr.merge.p95` | Pull requests | A pull request merge completes | 95 % ≤ 60000 ms | fast | 3 · pull request |
| `feed.latency` | Pull requests | A PR event reaches the activity feed | 99.9 % ≤ 10000 ms | fast | 3 · pull request |
| `reg.token.p95` | Container registry | Minting a registry bearer token completes | 95 % ≤ 300 ms | fast | 4 · registry |
| `reg.push.ok` | Container registry | Pushing an image succeeds | 99.9 % | fast | 4 · registry |
| `reg.manifest.p95` | Container registry | Fetching a manifest completes | 95 % ≤ 500 ms | fast | 4 · registry |
| `reg.tags.visible` | Container registry | A pushed tag becomes visible in the tag list | 99.9 % ≤ 5000 ms | fast | 4 · registry |
| `reg.shared.layer` | Container registry | A shared layer is not re-uploaded by a sibling image | 100 % | fast | 4 · registry |
| `reg.canary` | Container registry | The registry canary image pulls successfully | 100 % | fast | 4 · registry |
| `reg.visibility` | Container registry | Image visibility (public vs. private) is enforced | 100 % | fast | 4 · registry |
| `ws.create.p95` | Workspaces | Creating a workspace completes | 95 % ≤ 90000 ms | fast | 5 · workspace |
| `ws.exec.ok` | Workspaces | Exec into a running workspace pod succeeds | 99.9 % | fast | 5 · workspace |
| `homes.rw.p95` | Workspaces | A read/write round trip on the shared home completes | 95 % ≤ 200 ms | fast | 5 · workspace |
| `gw.tunnel.p95` | Workspaces | Opening a gateway SSH tunnel completes | 95 % ≤ 3000 ms | fast | 5 · workspace |
| `gw.unregistered.refused` | Workspaces | The gateway refuses an unregistered key | 99.9 % | fast | 5 · workspace |
| `ws.push.p95` | Workspaces | Pushing a workspace snapshot completes | 95 % ≤ 60000 ms | fast | 5 · workspace |
| `ws.clone.p95` | Workspaces | Cloning a workspace completes | 95 % ≤ 60000 ms | fast | 5 · workspace |
| `quota.refused` | Workspaces | An over-quota create is refused with 409 | 100 % | fast | 5 · workspace |
| `env.create.p95` | Environments | Creating an environment completes | 95 % ≤ 120000 ms | fast | 6 · environment |
| `env.dns` | Environments | Service-to-service DNS resolves inside an environment's namespace | 99.9 % | fast | 6 · environment |
| `env.attach` | Environments | Attaching a workspace to an environment takes effect | 99.9 % ≤ 5000 ms | fast | 6 · environment |
| `env.detach` | Environments | Detaching a workspace from an environment takes effect | 99.9 % ≤ 5000 ms | fast | 6 · environment |
| `env.push.p95` | Environments | Pushing an environment snapshot completes | 95 % ≤ 90000 ms | fast | 6 · environment |
| `ws.stop.p95` | Workspace lifecycle | Stopping a workspace completes | 95 % ≤ 15000 ms | fast | 7 · lifecycle |
| `ws.replicated` | Workspace lifecycle | A stopped workspace's final sync point reaches a replica | 99.9 % ≤ 300000 ms | fast | 7 · lifecycle |
| `ws.start.p95` | Workspace lifecycle | Starting a workspace completes | 95 % ≤ 30000 ms | fast | 7 · lifecycle |
| `ws.restore` | Workspace lifecycle | Restoring a workspace from a past snapshot succeeds | 99.9 % | fast | 7 · lifecycle |
| `vol.refusals` | Workspace lifecycle | Deleting a sync point or a running worktree's base snapshot is refused | 100 % | fast | 7 · lifecycle |
| `vol.detached.restorable` | Workspace lifecycle | A detached volume's snapshot can still be restored | 99.9 % | fast | 7 · lifecycle |
| `vol.orphan.collected` | Workspace lifecycle | An orphaned volume directory is collected | 99.9 % ≤ 300000 ms | fast | 7 · lifecycle |
| `req.queue` | Admin | A Request CR is queued and answerable by an admin | 99.9 % | fast | 8 · admin |
| `audit.row` | Admin | Every admin write produces an audit row | 100 % | fast | 8 · admin |
| `signals.fresh` | Admin | The Signals table reflects a rule transition | 99.9 % ≤ 60000 ms | fast | 8 · admin |
| `history.api` | Admin | The history API answers a chart query | 99.9 % | fast | 8 · admin |
| `sec.private.repo` | Security | A private repo is unreadable to a non-collaborator | 100 % | fast | 9 · security |
| `sec.cross.owner` | Security | One owner's objects are invisible to another owner | 100 % | fast | 9 · security |
| `sec.admin.claim` | Security | An admin route refuses a token without the superadmin claim | 100 % | fast | 9 · security |
| `sec.user.process` | Security | The ordinary API process has no admin route mounted | 100 % | fast | 9 · security |
| `sec.agent.spec` | Security | The admission policy refuses a spec write outside the allowed fields | 100 % | fast | 9 · security |
| `id.token.revoked` | Security | A revoked token is refused | 99.9 % | fast | 9 · security |
| `edge.dns` | Edge and pipeline | The public hostname resolves | 99.99 % | fast | 10 · edge |
| `edge.cert` | Edge and pipeline | The TLS certificate is valid for the public hostname | 99.9 % | fast | 10 · edge |
| `edge.origin` | Edge and pipeline | Cloudflare reaches the origin | 99.9 % | fast | 10 · edge |
| `edge.ssh.lb` | Edge and pipeline | The SSH load balancer accepts a connection | 99.9 % | fast | 10 · edge |
| `tel.log.latency` | Edge and pipeline | A structured log line reaches HyperDX | 99.9 % ≤ 30000 ms | fast | 10 · edge |
| `tel.pod.coverage` | Edge and pipeline | Every pod is scraped by the region's collector | 99.9 % ≤ 60000 ms | fast | 10 · edge |
| `tel.stream.lag` | Edge and pipeline | The Redis events stream consumer lag stays low | 99.9 % ≤ 60000 ms | fast | 10 · edge |
| `tel.ch.disk` | Edge and pipeline | ClickHouse disk usage is reported | 99.9 % ≤ 60000 ms | fast | 10 · edge |
| `git.push.large` | Git hosting | Push of a large commit over HTTP succeeds | 99.9 % | weekly | weekly · large git push |
| `reg.push.large` | Container registry | Pushing a large image layer succeeds | 99.9 % | weekly | weekly · large registry push |
| `ws.cold.profile` | Workspaces | A cold package profile builds successfully | 99.9 % | weekly | weekly · cold profile |
| `ws.profile.reuse` | Workspaces | A repeat package set is published from the profile index, not rebuilt | 99.9 % | weekly | weekly · profile reuse |
| `ws.cross.node` | Workspaces | A workspace started on a peer node reads its replica correctly | 99.9 % | weekly | weekly · cross-node start |
| `homes.cross.node` | Workspaces | The shared home is consistent across nodes | 99.9 % | weekly | weekly · cross-node home |
| `cp.failover` | Control plane | The leader lease fails over to another pod | 99.9 % | weekly | weekly · failover |
| `settings.live` | Control plane | A live settings change takes effect on the next beat | 99.9 % | weekly | weekly · live settings |
| `bak.tarball.age` | Backups | The latest backup tarball is recent | 99.9 % | monthly | monthly · backup tarball |
| `bak.daily.slots` | Backups | Every daily backup slot is present | 99.9 % | monthly | monthly · backup slots |
| `bak.versioning` | Backups | Backup versioning is enabled and retains history | 99.9 % | monthly | monthly · backup versioning |
| `bak.cosmos` | Backups | The Cosmos backup for HyperDX succeeds | 99.9 % | monthly | monthly · Cosmos backup |
| `drill.dead.node` | Resilience drills | A dead-node drill heals every replica onto a live node | 99.9 % | monthly | monthly · dead-node drill |
| `drill.drain` | Resilience drills | A drain drill succeeds without interrupting a running worktree | 99.9 % | monthly | monthly · drain drill |
| `drill.redis.down` | Resilience drills | The system keeps operating correctly with Redis down | 99.9 % | monthly | monthly · Redis-down drill |
| — | Identity | A person can reset access via the email magic link | — | manual | manual · email link |
| — | Identity | A person can register a new passkey | — | manual | manual · passkey registration |
| — | Backups | A full backup restore rebuilds a working cluster | — | manual | manual · restore drill |

# SLOs

One synthetic user, `kloudlite-slo`, walks this whole table as a Kubernetes `CronJob` — fast
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

The hourly suite (`14 · Experience`) is the owner's addendum: everything a person does that the
five-minute run has no room for — packages, teams and invitations, branch protection, a
two-service environment, an approved quota request. It runs the fast journey first, so every
hourly run is also a fast sample, and its burn pair is 24 h / 4 h because one sample an hour
cannot fill a five-minute window. It runs as its own tenant pair (`slo-hourly`/`slo-hourly-other`,
and the drills as `slo-drill*`) with its own SSH key, so a ~50-minute run never shares an SSH key,
a superadmin grant or a quota with the five-minute suite underneath it.

| id | Feature | SLI | Target | Suite | Journey step |
| --- | --- | --- | --- | --- | --- |
| `id.signin` | Identity | Sign-in over HTTP succeeds | 99.9 % | fast | 1 · Identity |
| `id.token.mint` | Identity | Minting a user JWT succeeds | 99.9 % | fast | 1 · Identity |
| `id.key.usable` | Identity | A freshly minted platform SSH key is usable | 99.9 % ≤ 30000 ms | fast | 1 · Identity |
| `id.cli.flow` | Identity | The kl CLI's login-to-command flow completes | 99.9 % ≤ 15000 ms | fast | 1 · Identity |
| `id.jwt.tiers` | Identity | A JWT is honoured across every tier | 99.9 % | fast | 1 · Identity |
| `id.signin.passkey` | Identity | A passkey registers, lists back and its sign-in lookup is peer-only | 99.9 % | fast | 1 · Identity |
| `git.push.ok` | Git hosting | Push of one commit over HTTP succeeds | 99.9 % | fast | 2 · Git |
| `git.push.p95` | Git hosting | Push of one commit over HTTP completes | 95 % ≤ 3000 ms | fast | 2 · Git |
| `git.clone.ok` | Git hosting | Clone over HTTP succeeds | 99.9 % | fast | 2 · Git |
| `git.clone.p95` | Git hosting | Clone over HTTP completes | 95 % ≤ 2000 ms | fast | 2 · Git |
| `ssh.clone.ok` | Git hosting | Clone over SSH succeeds | 99.9 % | fast | 2 · Git |
| `ssh.hostkey` | Git hosting | The SSH host key served matches the pinned fingerprint | 99.9 % | fast | 2 · Git |
| `ssh.unregistered.refused` | Git hosting | SSH from an unregistered key is refused | 99.9 % | fast | 2 · Git |
| `browse.p95` | Git hosting | The Browse API renders a repo page | 95 % ≤ 500 ms | fast | 2 · Git |
| `browse.commit.visible` | Git hosting | A pushed commit becomes visible in Browse | 99.9 % ≤ 5000 ms | fast | 2 · Git |
| `web.repo.page` | Git hosting | The web app's repo page loads | 95 % ≤ 1500 ms | fast | 2 · Git |
| `git.push.ssh` | Git hosting | Push of one commit over SSH succeeds | 99.9 % | fast | 2 · Git |
| `repo.lifecycle` | Git hosting | A repo is created, listed, deleted and its slug freed | 99.9 % ≤ 10000 ms | fast | 2 · Git |
| `web.org.page` | Git hosting | The web app's org page loads | 95 % ≤ 1500 ms | fast | 2 · Git |
| `web.repo.settings` | Git hosting | The web app's repo settings page loads | 95 % ≤ 1500 ms | fast | 2 · Git |
| `web.workspaces.page` | Workspaces | The web app's workspaces and environments pages load | 95 % ≤ 1500 ms | fast | 2 · Git |
| `pr.merge.p95` | Pull requests | A pull request merge completes | 95 % ≤ 60000 ms | fast | 3 · Pull request |
| `feed.latency` | Pull requests | A PR event reaches the activity feed | 99.9 % ≤ 30000 ms | fast | 3 · Pull request |
| `reg.token.p95` | Container registry | Minting a registry bearer token completes | 95 % ≤ 300 ms | fast | 4 · Registry |
| `reg.push.ok` | Container registry | Pushing an image succeeds | 99.9 % | fast | 4 · Registry |
| `reg.manifest.p95` | Container registry | Fetching a manifest completes | 95 % ≤ 500 ms | fast | 4 · Registry |
| `reg.tags.visible` | Container registry | A pushed tag becomes visible in the tag list | 99.9 % ≤ 5000 ms | fast | 4 · Registry |
| `reg.shared.layer` | Container registry | A shared layer is not re-uploaded by a sibling image | 99.9 % | fast | 4 · Registry |
| `reg.canary` | Container registry | The registry canary image pulls successfully | 99.9 % | fast | 4 · Registry |
| `reg.visibility` | Container registry | Image visibility (public vs. private) is enforced | 99.9 % | fast | 4 · Registry |
| `reg.image.delete` | Container registry | Deleting a tag removes it from the tag list and deleting an image removes it from the catalogue | 99.9 % ≤ 10000 ms | fast | 4 · Registry |
| `reg.catalogue` | Container registry | The image catalogue lists a pushed image from any node | 99.9 % ≤ 5000 ms | fast | 4 · Registry |
| `ws.create.p95` | Workspaces | Creating a workspace completes | 95 % ≤ 90000 ms | fast | 5 · Workspace |
| `ws.exec.ok` | Workspaces | Exec into a running workspace pod returns the command's output, from a pod whose home is the shared export | 99.9 % | fast | 5 · Workspace |
| `homes.rw.p95` | Workspaces | A read/write round trip on the shared home completes | 95 % ≤ 200 ms | fast | 5 · Workspace |
| `gw.tunnel.p95` | Workspaces | Opening a gateway SSH tunnel completes | 95 % ≤ 3000 ms | fast | 5 · Workspace |
| `gw.unregistered.refused` | Workspaces | The gateway refuses an unregistered key | 99.9 % | fast | 5 · Workspace |
| `ws.push.p95` | Workspaces | Pushing a workspace snapshot completes | 95 % ≤ 60000 ms | fast | 5 · Workspace |
| `ws.clone.p95` | Workspaces | Cloning a workspace completes | 95 % ≤ 60000 ms | fast | 5 · Workspace |
| `quota.refused` | Workspaces | An over-quota create is refused with 409 naming the dimension, what is used and the limit | 99.9 % | fast | 5 · Workspace |
| `env.quota.refused` | Workspaces | An over-quota restore, clone and push are each refused with 409 | 99.9 % | fast | 5 · Workspace |
| `env.create.p95` | Environments | Creating an environment completes | 95 % ≤ 120000 ms | fast | 6 · Environment |
| `env.dns` | Environments | A service in an environment resolves a sibling by bare name and connects to it | 99.9 % | fast | 6 · Environment |
| `env.attach` | Environments | Attaching a workspace to an environment takes effect | 99.9 % ≤ 10000 ms | fast | 6 · Environment |
| `env.detach` | Environments | Detaching a workspace from an environment takes effect | 99.9 % ≤ 10000 ms | fast | 6 · Environment |
| `env.push.p95` | Environments | Pushing an environment snapshot completes | 95 % ≤ 90000 ms | fast | 6 · Environment |
| `env.exec.ok` | Environments | Exec into a running service pod of the environment succeeds | 99.9 % | fast | 6 · Environment |
| `env.clone.p95` | Environments | Cloning a running environment completes with its services ready | 95 % ≤ 120000 ms | fast | 6 · Environment |
| `ws.stop.p95` | Workspace lifecycle | Stopping a workspace completes | 95 % ≤ 15000 ms | fast | 7 · Lifecycle |
| `ws.replicated` | Workspace lifecycle | A stopped workspace's final sync point reaches a replica, named by that replica | 99.9 % ≤ 300000 ms | fast | 7 · Lifecycle |
| `ws.start.p95` | Workspace lifecycle | Starting a workspace completes | 95 % ≤ 30000 ms | fast | 7 · Lifecycle |
| `ws.restore` | Workspace lifecycle | Restoring a workspace from a past snapshot succeeds | 99.9 % | fast | 7 · Lifecycle |
| `env.stop.p95` | Environments | Stopping an environment completes | 95 % ≤ 30000 ms | fast | 7 · Lifecycle |
| `env.replicated` | Environments | A stopped environment's final sync point reaches a replica | 99.9 % ≤ 300000 ms | fast | 7 · Lifecycle |
| `env.start.p95` | Environments | Starting an environment completes | 95 % ≤ 60000 ms | fast | 7 · Lifecycle |
| `env.restore` | Environments | Restoring an environment from a past snapshot succeeds | 99.9 % | fast | 7 · Lifecycle |
| `vol.refusals` | Workspace lifecycle | Deleting a sync point or a running worktree's base snapshot is refused | 99.9 % | fast | 7 · Lifecycle |
| `vol.detached.restorable` | Workspace lifecycle | A detached volume's snapshot can still be restored | 99.9 % | fast | 7 · Lifecycle |
| `vol.orphan.collected` | Workspace lifecycle | An orphaned volume directory is collected, and a Volume with no owner entry and no snapshot is deleted | 99.9 % ≤ 300000 ms | fast | 7 · Lifecycle |
| `wt.delete` | Workspace lifecycle | Deleting a workspace or environment drops the worktree and leaves the volume iff a snapshot remains | 99.9 % ≤ 60000 ms | fast | 7 · Lifecycle |
| `snap.delete` | Workspace lifecycle | Deleting a snapshot removes it from history, and the last one of a detached volume takes the volume with it | 99.9 % | fast | 7 · Lifecycle |
| `req.queue` | Admin | A Request CR is queued and answerable by an admin | 99.9 % ≤ 5000 ms | fast | 8 · Admin |
| `audit.row` | Admin | Every admin write produces an audit row, and the same write reaches `kloudlite.events` as `admin.<action>` | 99.9 % | fast | 8 · Admin |
| `signals.fresh` | Admin | The Signals table reflects a rule transition, and a rule with no covering samples reads `unknown` rather than `ok` | 99.9 % ≤ 120000 ms | fast | 8 · Admin |
| `history.api` | Admin | The history API answers a chart query | 99.9 % | fast | 8 · Admin |
| `sec.private.repo` | Security | A private repo is unreadable to a non-collaborator | 100 % | fast | 9 · Security |
| `sec.cross.owner` | Security | One owner's objects are invisible to another owner | 100 % | fast | 9 · Security |
| `sec.admin.claim` | Security | An admin route refuses a token without the superadmin claim | 100 % | fast | 9 · Security |
| `sec.user.process` | Security | The ordinary API process has no admin route mounted | 100 % | fast | 9 · Security |
| `sec.agent.spec` | Security | The admission policy refuses a spec write outside the allowed fields and still admits the allowed ones | 100 % | fast | 9 · Security |
| `id.token.revoked` | Security | A revoked token is refused | 99.9 % | fast | 9 · Security |
| `repo.visibility` | Security | Flipping a repo private hides it from a non-collaborator and flipping it public restores it | 100 % | fast | 9 · Security |
| `edge.dns` | Edge and pipeline | The public hostname resolves | 99.99 % | fast | 10 · Edge |
| `edge.cert` | Edge and pipeline | The TLS certificate is valid for the public hostname | 99.9 % | fast | 10 · Edge |
| `edge.origin` | Edge and pipeline | Cloudflare reaches the origin | 99.9 % | fast | 10 · Edge |
| `edge.ssh.lb` | Edge and pipeline | The SSH load balancer accepts a connection | 99.9 % | fast | 10 · Edge |
| `tel.log.latency` | Edge and pipeline | A structured log line reaches HyperDX | 99.9 % ≤ 60000 ms | fast | 10 · Edge |
| `tel.pod.coverage` | Edge and pipeline | Every pod is scraped by the region's collector | 99.9 % ≤ 60000 ms | fast | 10 · Edge |
| `tel.stream.lag` | Edge and pipeline | The Redis events stream consumer lag stays low | 99.9 % ≤ 60000 ms | fast | 10 · Edge |
| `tel.ch.disk` | Edge and pipeline | ClickHouse disk usage is reported | 99.9 % ≤ 60000 ms | fast | 10 · Edge |
| `git.push.large` | Git hosting | Push of a large commit over HTTP succeeds | 99.9 % | weekly | 12 · Weekly |
| `reg.push.large` | Container registry | Pushing a large image layer succeeds | 99.9 % | weekly | 12 · Weekly |
| `ws.cold.profile` | Workspaces | A cold package profile builds successfully | 99.9 % | weekly | 12 · Weekly |
| `ws.profile.reuse` | Workspaces | A repeat package set is published from the profile index, not rebuilt | 99.9 % | weekly | 12 · Weekly |
| `ws.cross.node` | Workspaces | A workspace started on a peer node reads its replica correctly | 99.9 % | weekly | 12 · Weekly |
| `homes.cross.node` | Workspaces | The shared home is consistent across nodes | 99.9 % | weekly | 12 · Weekly |
| `env.cross.node` | Environments | An environment started on a peer node reads its replica correctly | 99.9 % | weekly | 12 · Weekly |
| `cp.failover` | Control plane | The leader lease fails over to another pod | 99.9 % ≤ 30000 ms | weekly | 12 · Weekly |
| `settings.live` | Control plane | A live settings change takes effect on the next beat | 99.9 % ≤ 60000 ms | weekly | 12 · Weekly |
| `settings.revert` | Control plane | Reverting to a stored settings version restores it | 99.9 % ≤ 60000 ms | weekly | 12 · Weekly |
| `settings.roll` | Control plane | A Boot-marked save is refused with 409 while one of its readers is mid-rollout | 99.9 % | weekly | 12 · Weekly |
| `reg.gc.sweep` | Container registry | A blob a sibling image still references survives that image's deletion and a GC pass | 99.9 % | weekly | 12 · Weekly |
| `bak.tarball.age` | Backups | The latest backup tarball is recent | 99.9 % | monthly | 13 · Monthly |
| `bak.daily.slots` | Backups | Every daily backup slot is present | 99.9 % | monthly | 13 · Monthly |
| `bak.versioning` | Backups | Backup versioning is enabled and retains history | 99.9 % | monthly | 13 · Monthly |
| `bak.cosmos` | Backups | The Cosmos backup for HyperDX succeeds | 99.9 % | monthly | 13 · Monthly |
| `drill.dead.node` | Resilience drills | A dead-node drill heals every replica onto a live node | 99.9 % | monthly | 13 · Monthly |
| `drill.drain` | Resilience drills | A drain drill succeeds without interrupting a running worktree | 99.9 % | monthly | 13 · Monthly |
| `drill.redis.down` | Resilience drills | The system keeps operating correctly with Redis down | 99.9 % | monthly | 13 · Monthly |
| `cluster.decommission` | Resilience drills | A decommission is refused until the agent stamps `drained`, then cordons the node | 99.9 % | monthly | 13 · Monthly |
| `ws.packages.add` | Workspaces | Adding a package to a running workspace makes it runnable (`which`) | 95 % ≤ 180000 ms | hourly | 14 · Experience |
| `ws.packages.remove` | Workspaces | Removing it makes it disappear from the profile | 95 % ≤ 120000 ms | hourly | 14 · Experience |
| `ws.seeded` | Workspaces | A workspace created from a repo and branch has that clone checked out | 95 % ≤ 180000 ms | hourly | 14 · Experience |
| `key.platform.regenerate` | Identity | Regenerating the platform key keeps seeding working | 99.9 % | hourly | 14 · Experience |
| `team.create` | Teams | A team can be created by a person | 99.9 % | hourly | 14 · Experience |
| `team.invite.accept` | Teams | An invite is created, previewed and accepted once | 99.9 % ≤ 5000 ms | hourly | 14 · Experience |
| `team.role.set` | Teams | A member's role changes and is reflected in the profile | 99.9 % | hourly | 14 · Experience |
| `team.repo.shared` | Teams | A member clones a team repo; a non-member is refused | 99.9 % | hourly | 14 · Experience |
| `team.workspace` | Teams | A team workspace lands in the team namespace and starts | 95 % ≤ 90000 ms | hourly | 14 · Experience |
| `team.member.remove` | Teams | A removed member loses access to the team repo | 99.9 % | hourly | 14 · Experience |
| `team.delete` | Teams | Deleting the team removes its profile and refuses its slug | 99.9 % | hourly | 14 · Experience |
| `repo.protection` | Git hosting | A protected branch refuses a direct push and still merges via a PR | 99.9 % | hourly | 14 · Experience |
| `repo.commit.patch` | Git hosting | An edit made through the web commit endpoint lands in the log | 99.9 % ≤ 5000 ms | hourly | 14 · Experience |
| `repo.compare` | Git hosting | Comparing two branches lists the right commits | 99.9 % ≤ 1000 ms | hourly | 14 · Experience |
| `pr.comment` | Pull requests | A comment on a PR is readable back | 99.9 % | hourly | 14 · Experience |
| `pr.close` | Pull requests | A closed PR is refused a merge | 99.9 % | hourly | 14 · Experience |
| `commit.verify` | Git hosting | The signature endpoint answers for a pushed commit | 99.9 % ≤ 1000 ms | hourly | 14 · Experience |
| `env.services.multi` | Environments | An environment with two services has both ready and resolving each other | 95 % ≤ 180000 ms | hourly | 14 · Experience |
| `env.clone` | Environments | A stopped environment clones with all services ready | 95 % ≤ 180000 ms | hourly | 14 · Experience |
| `env.restore.inplace` | Environments | Restore in place brings a service's data back | 99.9 % | hourly | 14 · Experience |
| `env.stop.start` | Environments | Stop then start round trip | 95 % ≤ 120000 ms | hourly | 14 · Experience |
| `vol.history` | Workspace lifecycle | History lists pushes newest first with their messages; refs answer | 99.9 % ≤ 1000 ms | hourly | 14 · Experience |
| `quota.view` | Admin | `GET /v1/quota` reflects the objects the run holds | 99.9 % | hourly | 14 · Experience |
| `request.approve` | Admin | An approved quota request raises the quota and unblocks the refused create | 99.9 % ≤ 60000 ms | hourly | 14 · Experience |
| `admin.stop.workspace` | Admin | An admin stop is visible to the owner as `stopped` | 99.9 % ≤ 30000 ms | hourly | 14 · Experience |
| `superadmin.grant` | Security | Granting superadmin adds the account to the roster and revoking takes it off | 100 % | hourly | 14 · Experience |
| `feed.experience` | Pull requests | The feed shows the team and repo events of this run | 99.9 % ≤ 30000 ms | hourly | 14 · Experience |
| `home.persists` | Workspaces | A file written in one workspace is read from a fresh workspace's home, with the cache and state directories still local | 99.9 % | hourly | 14 · Experience |
| `id.username` | Identity | Claiming a username succeeds once and the second claim is refused | 99.9 % | hourly | 14 · Experience |
| `id.cli.tokens` | Identity | A CLI token is listed and, once revoked, is refused | 99.9 % | hourly | 14 · Experience |
| `id.profile.upsert` | Identity | A profile upsert is saved and read back | 99.9 % ≤ 5000 ms | hourly | 14 · Experience |
| `id.cli.sshconfig` | Identity | `kl ws sshconfig` writes a host block naming a running workspace | 99.9 % ≤ 15000 ms | hourly | 14 · Experience |
| `key.ssh.lifecycle` | Identity | A newly added SSH key clones, and after removal the same key is refused | 99.9 % ≤ 30000 ms | hourly | 14 · Experience |
| `repo.description` | Git hosting | A repo description is saved and read back | 99.9 % ≤ 5000 ms | hourly | 14 · Experience |
| `pr.merge.strategies` | Pull requests | Each merge strategy — merge, squash, rebase, fast-forward — lands the expected tree | 99.9 % | hourly | 14 · Experience |
| `pr.mergeability` | Pull requests | Mergeability is reported clean for a clean change and dirty for a conflicting one | 99.9 % ≤ 30000 ms | hourly | 14 · Experience |
| `team.invite.revoke` | Teams | A revoked invite token is refused | 100 % | hourly | 14 · Experience |
| `team.environment` | Teams | A team environment lands in the team namespace and its services resolve | 95 % ≤ 180000 ms | hourly | 14 · Experience |
| `env.attach.pair` | Environments | Deleting an attached workspace removes the environment-side policy | 99.9 % ≤ 30000 ms | hourly | 14 · Experience |
| `vol.list` | Workspace lifecycle | The volume list names every volume the run holds | 99.9 % | hourly | 14 · Experience |
| `admin.stop.environment` | Admin | An admin stop of an environment is visible to the owner as `stopped` | 99.9 % ≤ 30000 ms | hourly | 14 · Experience |
| `admin.delete.workload` | Admin | An admin delete takes a workspace and an environment away | 99.9 % ≤ 60000 ms | hourly | 14 · Experience |
| `admin.screens` | Admin | The owners, clusters and overview console screens answer | 99.9 % ≤ 10000 ms | hourly | 14 · Experience |
| `admin.workloads.read` | Admin | `GET /admin/workloads` lists every roll target | 99.9 % ≤ 5000 ms | hourly | 14 · Experience |
| `audit.export` | Admin | The audit CSV export answers with a header and this run's rows | 99.9 % ≤ 10000 ms | hourly | 14 · Experience |
| `req.decide.kinds` | Admin | An access request grants membership and a denied request is closed with its reason | 99.9 % ≤ 60000 ms | hourly | 14 · Experience |
| `req.legacy.union` | Admin | The retired quota-request queue is unioned into the admin queue and migrates | 99.9 % ≤ 10000 ms | hourly | 14 · Experience |
| `region.status` | Admin | The region list and this run's cluster status answer | 99.9 % ≤ 5000 ms | hourly | 14 · Experience |
| — | Identity | A person can reset access via the email magic link | — | manual | manual · email link |
| — | Identity | A person can register a new passkey | — | manual | manual · passkey registration |
| — | Backups | A full backup restore rebuilds a working cluster | — | manual | manual · restore drill |

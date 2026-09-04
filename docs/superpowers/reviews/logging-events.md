# Log events

Every `tracing` call in this repository emits an EVENT, not a sentence. The event name is the
`message`; everything specific is a field. This is what makes a postmortem possible: one name per
kind of thing that happens, the same field names everywhere, and a level that means the same thing
in every crate. The collectors parse the JSON form (`KLOUDLITE_GIT_LOG_FORMAT=json`) into columns,
so the name is what HyperDX's pattern view groups on and the fields are what you filter on.

## The name

`subject.verb[.qualifier]`, lower-case, dots only. Subject is the thing the event is about
(`listener`, `process`, `lease`, `ownership`, `gc`, `reconcile`, `merge`, `push`, `snapshot`,
`volume`, `workspace`, `environment`, `node`, `settings`, `directory`, `history`, `alerts`,
`registry`, `tunnel`, `home`, `pool`). Verb is past tense for something that happened
(`started`, `stopped`, `completed`, `failed`, `skipped`, `refused`, `retried`), present for a state
being reported (`unavailable`, `degraded`). A pair is `.begun` / `.completed`, and `.completed`
carries `duration_ms`. Never an id, a count, a path or an address inside the name.

## The fields

One name per concept, used identically in every crate:

| field | meaning |
|---|---|
| `error` | the failure, `%e` (Display), never `?e` |
| `repo`, `owner`, `name` | a git repo (`owner`/`name` split) or an image |
| `workspace`, `environment`, `volume`, `snapshot`, `binding` | the CR's name |
| `node`, `region` | cluster placement |
| `kind` | a CRD kind or a request kind |
| `listener` | `http`, `ssh`, `peer`, `metrics`, `api`, `admin` |
| `addr` | a bound socket address |
| `attempt`, `duration_ms`, `count`, `bytes` | numbers, never formatted into the message |
| `signal` | `sigterm`, `sigint` |
| `mode` | a chosen configuration branch (`env-only`, `plain-http`, `sandboxed`) |
| `reason` | why a refusal or skip happened, short and stable |
| `op` | the underlying operation a generic wrapper was performing (`xadd`, `xack`) |
| `role` | `reader` or `writer` |
| `step` | which stage of a multi-step sequence (shutdown) an event is about |
| `scope` | which settings document (`central`, a region) |

Fields are `tracing` key-value pairs: `info!(listener = "http", %addr, "listener.started")`.

## The level

- `debug` — a routine beat did its routine thing: sync cuts, GC sweeps that found nothing new,
  checkpoints, marker reconciles, lease renewals that succeeded.
- `info` — a lifecycle fact or a state transition someone would want on a timeline: a listener
  up, a shutdown begun and completed, a node's roles, a mode chosen at boot, a volume moved, a
  push completed, a merge landed, a region activated.
- `warn` — a failure that will be retried or that degrades one thing while the process keeps
  serving. Carries `error` and the object.
- `error` — a human has to act: data-invariant violations, a listener that died, a boot failure,
  a sweep aborted on unreadable input.

Expected outcomes (a 404 for a missing object, a refused request, an idle tick) are `debug` at
most. A condition that is true by design on this deployment (no TLS dir because TLS is at the
edge, no object store on the gateway) is `info` with `mode`, logged once at boot, never `warn`.

## One event per happening

Log where the decision is made, once. A failure propagated to a caller that also logs is logged
by the OUTERMOST site only. A begin/end pair is two events; never a third saying the same thing.
Boot emits one `listener.started` per listener and one `process.started` with the resolved modes,
not a sentence that lists them.

## Catalogue

The names in use, so a new one is checked against the list before it is coined. Add a row when
you add an event.

| event | level | fields |
|---|---|---|
| `process.started` | info | `service`, `version`, modes as fields |
| `process.shutdown.begun` | info | `signal` |
| `process.shutdown.completed` | info | `duration_ms` |
| `process.exiting` | error | `error` |
| `listener.started` | info | `listener`, `addr` |
| `listener.failed` | error | `listener`, `addr`, `error` |
| `settings.central.unavailable` | info (boot, by design) / warn (was available) | `mode`, `error` |
| `settings.reloaded` | info | `scope`, `version` |
| `settings.invalid` | warn | `scope`, `error` |
| `lease.acquired` / `lease.released` | info | `epoch` |
| `lease.renew.failed` | warn | `error`, `attempt` |
| `election.tick.failed` | warn | `error` |
| `ownership.checkpoint.completed` | debug | `duration_ms` |
| `ownership.read.failed` | warn | `repo`, `error` |
| `ownership.granted` | debug | `repo`, `node` |
| `gc.markers.reconciled` | debug | `owner`, `count` |
| `gc.listing.failed` | warn | `prefix`, `error` |
| `gc.sweep.completed` | info when `count > 0`, else debug | `count`, `duration_ms` |
| `gc.sweep.aborted` | error | `reason`, `error` |
| `directory.sweep.completed` | debug | `count` |
| `directory.connected` | info | `db` |
| `merge.completed` / `merge.failed` | info / warn | `owner`, `name`, `number`, `strategy`, `error` |
| `push.completed` / `push.failed` | info / warn | `workspace` or `environment`, `snapshot`, `error` |
| `reconcile.failed` | warn | `kind`, `name`, `error` |
| `reconcile.queue.failed` | warn | `kind`, `error` |
| `claim.completed` / `claim.refused` | info / debug | `kind`, `name`, `node`, `reason` |
| `volume.moved` / `volume.released` / `volume.taken` | info | `volume`, `node`, `reason` |
| `snapshot.cut` / `snapshot.pruned` | debug | `workspace`/`environment`, `snapshot` |
| `sync.cut` | debug | `workspace`, `snapshot` |
| `home.mounted` / `home.remounting` / `home.mount.failed` | info / warn / error | `export`, `error` |
| `node.roles` | info | `node`, `roles` |
| `node.draining` / `node.drained` | info | `node`, counts |
| `tunnel.opened` / `tunnel.closed` / `tunnel.refused` | info / info / debug | `workspace`, `duration_ms`, `reason` |
| `registry.blob.deleted` / `registry.marker.refresh.failed` | info / warn | `owner`, `name`, `digest`, `error` |
| `history.migrations.applied` / `history.migrations.failed` | info / error | `count`, `error` |
| `history.watch.restarted` | warn once, then debug | `kind`, `region`, `error` |
| `history.write.failed` | warn | `table`, `count`, `error` |
| `alerts.write.failed` | warn | `count`, `error` |
| `process.shutdown.stalled` | warn | `step`, `timeout_s` |
| `store.multipart.unavailable` | info (boot, by design) | `url`, `mode` |
| `cache.unavailable` / `cache.script.failed` | warn | `host` |
| `cache.stream.failed` | warn | `stream`, `group`, `op`, `count`, `error` |
| `ownership.map.opened` | info | `path`, `role` |
| `ownership.open.failed` / `ownership.close.failed` | warn | `role`, `error` |
| `ownership.checkpoint.failed` | warn | `reason`, `timeout_s`, `error` |
| `ownership.prune.failed` | warn | `error` |
| `ownership.claim.failed` / `ownership.release.failed` | warn | `repo`, `reason`, `error` |
| `ownership.lost` | info | `repo` |
| `lease.demoted` | warn | `epoch`, `reason` |
| `lease.read.failed` / `lease.release.failed` | warn | `error` |
| `pool.close.failed` | error | `repo`, `reason`, `error` |
| `pool.close.stalled` / `pool.flush.stalled` | warn | `repo`, `timeout_ms` |
| `pool.flush.failed` | warn | `repo`, `error` |
| `pool.release_hook.missing` | error | `count` |
| `route.forward.failed` | error | `repo`, `peer`, `error` |
| `route.claim.failed` | warn | `repo`, `reason`, `error` |
| `request.failed` | error | `error` |
| `browse.not_found` | debug | `error` |
| `repo.open.failed` / `repo.create.failed` / `repo.delete.failed` / `repo.visibility.failed` / `repo.protection.save.failed` | error | `owner`, `repo`, `error` |
| `index.write.failed` / `index.marker.reconcile.failed` | warn | `owner`, `repo`, `reason`, `error` |
| `packs.consolidated` | info | `owner`, `repo`, `before`, `after` |
| `packs.consolidate.failed` / `packs.index.read.failed` / `packs.cache.prune.failed` | warn | `owner`, `repo`, `error` |
| `merge.record.failed` | error | `owner`, `repo`, `error` |
| `merge.mergeability.failed` / `merge.stranded.scan.failed` / `merge.announce.failed` | warn | `owner`, `repo`, `number`, `error` |
| `peer.stream.failed` | warn | `error` |
| `receive.options` | debug | `options` |
| `receive.pack.failed` | error | `error` |
| `directory.unavailable` | warn | `error` |
| `directory.repair.completed` / `directory.repair.failed` | info / warn | `count`, `error` |
| `directory.sweep.failed` | warn | `error` |
| `registry.tag.read.failed` / `registry.counter.write.failed` / `registry.counters.flush.failed` / `registry.blob.rows.failed` / `registry.manifest.skipped` | warn | `owner`, `name`, `tag`/`digest`/`manifest`, `reason`, `error` |
| `superadmin.acting` | info | `caller`, `owner` |
| `tls.mode` | info (boot, by design) | `mode` |
| `tunnel.dial.failed` | warn | `workspace`, `error` |
| `merge.unavailable` | error | `reason` |
| `merge.sync.failed` | warn | `owner`, `name`, `error` |
| `gc.sweep.failed` | warn | `owner`, `error` |
| `gc.markers.reconcile.failed` | warn | `owner`, `kind`, `error` |
| `gc.cache.pruned` | info | `count` |
| `gc.uploads.swept` / `gc.uploads.failed` | info / warn | `owner`, `count`, `error` |
| `directory.read.failed` / `directory.write.failed` / `directory.request.failed` | error, or warn where the caller degrades one field | `reason`, `owner`/`team`/`user`/`handle`, `error` |
| `directory.superadmins.seeded` / `directory.superadmins.seed.failed` | info / warn | `count`, `error` |
| `auth.signing.unavailable` | warn | `reason` |
| `auth.token.mint.failed` | error | `error` |
| `kube.unavailable` | warn | `reason`, `error` |
| `history.watch.skipped` / `history.unavailable` | warn | `reason` |
| `feed.read.failed` / `browse.read.failed` | error | `reason`, `owner`, `error` |
| `passkey.read.failed` / `passkey.write.failed` | error | `reason`, `error` |
| `credential.read.failed` / `credential.write.failed` / `credential.create.failed` / `credential.revoke.failed` / `credential.unwind.failed` / `credential.forget.failed` | error, warn for the unwind/forget best-effort pair | `reason`, `owner`, `credential`/`jti`, `error` |
| `sshkey.add.failed` / `sshkey.read.failed` | error | `owner`, `error` |
| `key.platform.installed` / `key.platform.failed` | info / error | `owner`, `replaced`, `reason` |
| `team.deleted` | info | `team`, `by` |
| `audit.write.failed` | error | `actor`, `action`, `target`, `error` |
| `repo.read.failed` / `repo.list.failed` | error | `owner`, `team`, `reason`, `error` |
| `upstream.request.failed` / `upstream.body.failed` / `upstream.parse.failed` | error | `reason`, `owner`, `name`, `repo`, `number`, `status`, `error` |
| `listing.failed` | warn | `kind`, `volume`/`owner`/`environment`/`node`, `reason`, `error` |
| `sweep.skipped` | warn | `node`, `reason` |
| `peer.client.failed` | warn | `error` |
| `peer.addr.failed` | warn | `volume`, `node`, `error` |
| `pull.retried` | warn | `volume`, `snapshot`, `node`, `reason` |
| `pull.failed` | warn | `volume`, `snapshot`, `node`, `reason`, `bytes`, `error` |
| `snapshot.cut.failed` | warn | `snapshot`, `error` |
| `snapshot.generation.failed` | warn | `snapshot`, `reason`, `error` |
| `snapshot.dropped` | info | `volume`, `snapshot`, `reason` |
| `snapshot.drop.failed` | warn | `volume`, `snapshot`, `reason`, `error` |
| `snapshot.prune.failed` | warn | `volume`, `snapshot`, `error` |
| `snapshot.send.failed` | warn | `volume`, `snapshot`, `status`, `stderr` |
| `snapshot.state.invalid` | warn | `value`, `error` |
| `replica.deleted` | info | `volume`, `name`, `reason` |
| `replica.delete.failed` | warn | `volume`, `name`, `reason`, `error` |
| `replica.status.write.failed` | warn | `volume`, `error` |
| `volume.release.failed` | warn | `volume`, `error` |
| `volume.mark.failed` | warn | `volume`, `reason`, `error` |
| `volume.dropped` | info | `volume`, `reason` |
| `volume.drop.failed` | warn | `volume`, `reason`, `error` |
| `volume.collected` | info | `volume`, `reason` |
| `volume.collect.failed` | warn | `volume`, `error` |
| `volume.cleanup.failed` | warn | `volume`, `path`, `reason`, `error` |
| `volume.delete.waiting` | info | `volume`, `reason` |
| `parent.mark.failed` | warn | `kind`, `name`, `reason`, `error` |
| `worktree.dropped` | info | `volume`, `count` |
| `worktree.drop.failed` | warn | `volume`, `reason`, `error` |
| `sync.skipped` | debug | `name`, `reason` |
| `sync.cut.failed` | warn | `name`, `error` |
| `sync.generation.failed` | warn | `name`, `reason`, `error` |
| `wake.failed` | warn | `node`, `status`, `reason`, `error` |
| `claim.retried` | debug | `kind`, `name`, `reason` |
| `reconcile.abandoned` | warn | `kind`, `name`, `reason`, `error` |
| `heartbeat.failed` | error | `error` |
| `labels.healed` | debug | `name`, `owner` |
| `node.read.failed` | warn | `node`, `reason`, `error` |
| `node.labels.missing` | warn | `node`, `reason` |
| `node.annotate.failed` | warn | `node`, `error` |
| `sandbox.mode` | info | `mode`, `runtime_class` |
| `nix.gcroot.missing` / `nix.gcroot.failed` | warn | `reason`, `error` |
| `nix.profiles.dir.failed` | warn | `error` |
| `nix.gc.completed` / `nix.gc.failed` | info / warn | `bytes`, `freed`, `error` |
| `janitor.reclaimed` | info | `attach`, `profiles` |
| `janitor.beat.failed` | warn | `error` |
| `home.oversized` | warn | `owner`, `bytes` |
| `homecache.not_subvolume` | warn | `path`, `reason` |
| `settings.unavailable` | warn | `scope`, `mode`, `error` |
| `settings.status.write.failed` | warn | `scope`, `error` |
| `settings.forward.failed` | error | `scope`, `reason`, `error` |
| `profile.index.failed` / `profile.remove.failed` | warn | `workspace`/`volume`, `error` |
| `workspace.rebuilding` | info | `workspace`, `reason` |
| `workspace.hostkey.missing` | warn | `workspace`, `name` |
| `workspace.keys.deferred` | info | `owner`, `workspace`, `reason` |
| `quota.defaulted` | info | `owner`, `reason` |
| `namespace.skipped` | warn | `owner`, `name`, `reason` |
| `key.read.failed` / `key.install.failed` | warn | `owner`, `reason`, `error` |
| `ssh.session.mint.failed` | error | `error` |
| `attach.clear.failed` / `attach.policy.delete.failed` | warn | `workspace`, `environment`, `error` |
| `audit.write.failed` | warn (no store) / error | `actor`, `action`, `target`, `reason`, `error` |
| `audit.read.failed` | warn | `name` |
| `workload.rolled` | info | `scope`, `name`, `by`, `reason` |
| `history.beats.skipped` | warn once, then debug | `table`, `reason` |
| `history.consumer.disabled` | info (boot, by design) | `mode` |
| `alerts.skipped` | warn | `region`, `reason` |

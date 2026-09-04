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
| `superadmin.acting` | info | `caller`, `owner` |

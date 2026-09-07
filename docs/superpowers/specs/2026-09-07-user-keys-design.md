# SSH keys belong to the person

Status: draft for review, 2026-09-07.

## Why

Today an SSH key is a credential row keyed by an **owner handle** (`Credential.owner`), a person
or a team, and every place that consumes keys holds a derived copy of that row:

| Consumer | Copy | Refreshed by |
| --- | --- | --- |
| git over SSH (`crates/git/src/ssh.rs`) | `auth/sshkey/{fingerprint}` → owner, in the object store | `add_ssh_key` on add, nothing on remove |
| workspace pods (`k8s::user_key_secret`) | Secret `user-key` per owner namespace with `authorized_keys` | the `on_keys_changed` hook, one Secret write per namespace per region, best effort |

Three consequences the change removes:

- A key registered under a team is a **shared identity**. `Conn.owner` becomes the team, the push
  is recorded as the team's, and nothing says which person made it.
- A person who belongs to N teams registers the same key N+1 times, and the fleet keeps N+1 copies
  of it in N+1 Secrets.
- Every copy is a place that can be stale: a hook lost mid-write, a Secret written for a namespace
  the api did not know about yet, a removed key whose fingerprint row nobody deleted.

The redesign has one source of truth, verification at the moment of use, and a single durable
projection per region that a controller converges, with a status anyone can read.

## Design

### 1. The directory holds keys per person

`Credential` rows of kind `SshKey` carry `owner = <the person's email>` (the same identity the
JWT `sub` and the directory key on), never a team handle. `created_by` stays (equal to `owner`
for keys, kept for tokens). `/v1/keys` (`POST`, `GET`, `DELETE`) drop the `owner` field and act
on the caller; a body that still sends `owner` is refused with 400 so an old CLI cannot register
a key under a team by accident. `SigningKey` follows the same rule (a commit is signed by a
person). `Token` and `CliToken` are **not** changed by this spec: a token is a scoped credential
for a namespace, not an identity, and `bins/server` authorizes tokens by owner today.

### 2. git over SSH verifies at the moment of use

The fingerprint index `auth/sshkey/{fp}` maps to the person's email. On `auth_publickey`, `Conn`
records the **user**, not an owner. Authorization for `owner/name` becomes: `user == owner`, or
`user` is a member of team `owner` with a role that permits the verb. The server tier already
holds the directory URI (the pull-request migration read); it gains a `Membership` reader with
the same one-minute cache `crates/api/src/browse.rs` uses, and refuses on a directory error
(never falls open). Removing a key deletes its fingerprint row in the same request as the
directory row, before the response, so the SSH path cannot accept a key the list no longer shows.

A key that is registered as both an SSH key and a signing key keeps two rows, as now.

### 3. Workspaces: one projection per owner per region, converged by the agent

A new cluster-scoped CRD, `OwnerKeys` (`kloudlite.io/v1alpha1`), named by the owner handle:

```yaml
apiVersion: kloudlite.io/v1alpha1
kind: OwnerKeys
metadata:
  name: acme                      # owner handle: a person's or a team's
spec:
  generation: 17                  # the directory's version of this set, monotonic
  authorizedKeys: |               # union of the members' SshKey rows, one OpenSSH line each
    ssh-ed25519 AAAA... alice@laptop
    ssh-ed25519 AAAA... bob@desk
status:
  observedGeneration: 17
  conditions:
    - type: Synced                # True when every pod of this owner on this node reads gen 17
      status: "True"
      reason: Applied
      lastTransitionTime: 2026-09-07T16:20:00Z
```

**Writer: the api only.** The api computes `authorizedKeys` from the directory (a person: their
keys; a team: the union of its members' keys) and applies the object with server-side apply,
`force`, field manager `kloudlite-api`, into every region the owner has a namespace in, exactly
as it writes `Workspace` and `Region` today. It writes on every key add/remove and on every
membership change, and, independently, a **resync beat** every `KEYS_RESYNC_SECS` (default 300)
recomputes every owner it knows and re-applies. A missed event is therefore at most one beat
late, never lost. The api never reads the CR to decide anything; the directory is the truth.

**Reader: the agent.** A reflector on `OwnerKeys`. For each owner with pods on this node it
writes `{pool}/keys/{owner}/authorized_keys` **in place** (write to the same inode, `0600`, owner
`kl`'s uid), the way the attach `resolv.conf` is written, and every workspace pod mounts that
file read-only through a hostPath volume of `type: File` at `/home/kl/.ssh/authorized_keys`.
sshd reads the file per login, so a change is live on the next connection with no pod restart
and no Secret. The agent then stamps `status.observedGeneration` and `Synced` through the
`/status` subresource. RBAC: the agent gets `get/list/watch` on `OwnerKeys` and `patch` on its
status only; the ValidatingAdmissionPolicy keeps it out of `spec`, as for every other kind.

**A missing or empty projection locks the owner out; it never opens the door.** No `OwnerKeys`
object, an empty `authorizedKeys`, or an unparsable spec means the agent writes an empty file
(and `Synced=False/NoKeys`), and sshd refuses every key. That is the same rule `write_user_key`
follows today ("a failed lookup writes NOTHING rather than an empty file") applied at the other
end: the failure mode is a login that fails loudly, not one that succeeds for a removed key.

The `user-key` Secret keeps only the platform-issued private key (`/v1/platform-key`), which is
per owner by design; its `authorized_keys` entry and `authorized_keys_volume()` go away.

### 4. The web

`/settings` lists and adds SSH and signing keys under the person, with no owner picker. A team's
settings page shows nothing about keys: what a team has is members, and their keys follow them.
`OwnerKeys.status` surfaces on the superadmin Clusters tab as one row per owner per region
(`Synced`, generation, age), which is the operator's answer to "is this key live everywhere".

### 5. Migration

One-shot, in `bins/api` at boot behind a version marker in the directory (`meta.keys_v2`):

1. Every `SshKey`/`SigningKey` row whose `owner` is a team handle is rewritten with
   `owner = created_by`. A person who registered the same key under two handles ends with one
   row (same id).
2. Every fingerprint row in the object store is rewritten to the person's email.
3. The first resync beat writes `OwnerKeys` for every owner in every region.
4. The `authorized_keys` entry is dropped from every `user-key` Secret on the beat after the
   agent reports `Synced=True` for that owner, so a region running an old agent keeps working
   until it is upgraded.

Rollout order: server tier (reads user fingerprints, membership check), then agents (CRD +
file mount), then the api (migration + writer). Each step is compatible with the previous
state, so a half-rolled fleet is never locked out.

## Failure modes, and what each does

| Failure | Behaviour |
| --- | --- |
| Directory unreachable during SSH auth | refuse the connection (never falls open) |
| Key removed, fingerprint row delete fails | request fails 502; the key stays listed and stays valid, and the person retries |
| Api's CR write fails | logged; the next resync beat rewrites it |
| Agent down when a key changes | the CR waits; the reflector converges on restart; `Synced=False` until then |
| Pod started before the file exists | the mount is `type: File`; the pod is parked `Creating`/`KeysNotReady` until the agent has written it, as `HomeNotReady` parks one today |
| `OwnerKeys` deleted by hand | agent writes an empty file, `Synced=False/NoKeys`; the next beat recreates it |
| Region added after a key | the beat writes the projection into the new region within `KEYS_RESYNC_SECS` |

## Out of scope

Tokens per person (would need the same membership check on the server; a later spec), key
expiry, and the gateway acting as the only authenticator (the tunnel already needs an api-minted
session token, so the in-pod `authorized_keys` is a second lock; removing it is a separate
decision).

## Files

`crates/pulls/src/directory/mod.rs` (credential owner semantics, migration), `crates/api/src/credentials.rs`
(no owner on key routes), `crates/storage/src/auth.rs` (fingerprint → user, delete), `bins/server`
(membership reader, SSH authorization), `crates/git/src/ssh.rs` (`Conn.user`),
`crates/workspaces/src/crd/mod.rs` (`OwnerKeys`), `crates/workspaces/src/api/workspaces.rs`
(writer + resync beat), `bins/agent/src/controller/keys.rs` (new), `crates/workspaces/src/k8s.rs`
(hostPath File mount, Secret without `authorized_keys`), `deploy/k3s/{crds,agent-rbac,agent-admission}.yaml`,
`web/apps/web/src/app/(shell)/settings/*`, superadmin Clusters tab.

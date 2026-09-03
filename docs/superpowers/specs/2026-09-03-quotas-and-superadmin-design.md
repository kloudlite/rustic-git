# Quotas, quota requests, and superadmin

**Date:** 2026-09-03
**Status:** approved by the owner (defaults accepted; only team admins may request on a team's behalf)

## Problem

Any authenticated account can create workspaces and environments without limit (review
2026-09-03, C2), disk is capped only per object, and the only "admin" is an email allowlist env var
read by `/v1/regions`. Teams exist (a workspace or environment can be owned by a team slug;
membership and roles live in the server tier's directory, `crates/api/src/teams.rs` `Role`), but
nothing is enforced per team.

## Decisions

### 1. One quota per owner

A cluster-scoped `Quota` CR, name = the owner slug (a user or a team). Spec:

```
workspaces: u32      # live working copies of kind Workspace
environments: u32    # live working copies of kind Environment
snapshots: u32       # snapshots (transient: false) on volumes the owner owns
diskGb: u64          # sum of Volume.spec.quotaGb over every volume the owner owns, detached included
cpu: u32             # sum of Workspace resources.cpuLimit + every environment service's cpuLimit, live only
memoryGb: u32        # same for memory
```

A single `Quota` named `default` is the fallback for any owner with no object of their own.
Bootstrap values (owner, 2026-09-03):

| | person | team |
|---|---|---|
| workspaces | 5 | 20 |
| environments | 2 | 8 |
| snapshots | 20 | 80 |
| diskGb | 100 | 400 |
| cpu | 8 | 32 |
| memoryGb | 32 | 128 |

Two defaults, `default-user` and `default-team`, since a slug does not say which it is; `/v1`
knows (a team slug appears in the directory). Only a superadmin may write a `Quota`.

Usage is computed, never stored: `/v1` lists the owner's Workspaces, Environments, Volumes and
Snapshots (already label-indexed by owner) and sums. `GET /v1/quota?owner=<slug>` returns
`{ limit, used }` per dimension; callable by the owner, a team member for their team, a superadmin
for anyone.

### 2. Enforcement

`/v1` refuses with `409` and a sentence naming the dimension, the limit and current use
("workspaces: 5 of 5 in use; request more under Quota") at: create workspace / environment,
restore, clone, push (snapshots), changing a volume's quota, and changing resources. The check
is read-then-write, so two concurrent creates can overshoot by one; accepted, the platform-side
cap below is the hard stop for the dimensions that matter.

Kubernetes `ResourceQuota` per owner namespace (`ws-<owner>`, `wt-<team>-…`, and each `env-<id>`
namespace of that owner) for `limits.cpu` and `limits.memory`, written by the agent when it
ensures the namespace (`OwnerBinding` path), from the owner's effective `Quota`. Disk has no
platform cap (btrfs qgroups are per volume already); counts have none.

### 3. Quota requests

A cluster-scoped `QuotaRequest` CR: `spec { owner, requested: <same six fields, all optional>,
reason }`, `status { state: pending | approved | denied, decidedBy, decidedAt, note }`.

- Who may create: the owner themself for a personal quota; for a team, only a member whose
  directory role is at least admin (`Role` rank in `crates/api/src/teams.rs`); `/v1` checks the
  role via the `Directory` trait, which gains `team_role(user, team) -> Option<Role>`.
- Approve/deny: superadmin only. Approving writes the `Quota` (creating it from the default if
  absent) with the requested values, then marks the request. Denying marks it with a note.
- One pending request per owner at a time (409 otherwise). Requests are never deleted by the
  system; the web shows the last few.
- The 409 from enforcement links to the request form.

### 4. Superadmin

- A `superadmins` collection/flag in the directory (server tier, beside users and teams):
  a list of user ids. Managed by `POST/DELETE /api/admin/superadmins/{user}` on the server tier,
  itself superadmin-only; the bootstrap is the existing `RUSTIC_GIT_WORKSPACES_ADMINS` env — on
  boot the api tier ensures those emails are in the list, then the env is only a bootstrap.
- Login mints `superadmin: true` into the session JWT when the user is listed; `/v1` and the
  web read the claim (`/v1` keeps `require_admin` but it now checks the claim, and `/v1/regions`
  moves under it too).
- The web gets an `/admin` area, visible only with the claim: quota requests queue (approve /
  deny with note), every owner's usage vs limit, the two defaults (editable), regions, the
  decommission status of each node (`rustic-git.io/decommission-status`).
- A superadmin can act on any owner's objects through `/v1` (list, stop, delete) — `may_act_on`
  gains the claim as a third arm — so support can clean up without impersonation.

### 5. Admin APIs live on their own server (owner, 2026-09-03)

Everything that needs the superadmin claim is served by a SEPARATE api process, never by the
`/v1` server:

| admin server (`/admin/*`) | user server (`/v1/*`) |
|---|---|
| regions: create, list all | regions: list active (read) |
| quota defaults: read, write | own quota: read (`GET /v1/quota`) |
| quota requests: list all, approve, deny | own request: create, read own |
| superadmin list: add, remove | — |
| every owner's usage | — |
| node decommission status | — |
| cross-owner list / stop / delete | own objects only |

- Same image, same `rustic-git-api` binary, one env `RUSTIC_GIT_API_ROLE=user|admin` (default
  `user`). The admin role mounts ONLY `/admin/*` and answers 404 to `/v1`; the user role mounts
  ONLY `/v1/*` and has no admin route compiled into its router — a `/v1` authorization bug cannot
  reach an admin handler because the handler is not there.
- The admin server refuses every request whose JWT lacks `superadmin: true`, before routing.
- Separate Deployment (`rustic-git-admin`, 1 replica) and Service — NO Ingress and no DNS
  (owner, 2026-09-04): nothing outside the cluster calls the admin api; the web reaches it
  server-side through `RUSTIC_GIT_ADMIN_API_URL=http://rustic-git-admin`, and the superadmin
  pages live on the app host at `/superadmin`, reached from a "Superadmin" entry in the profile
  dropdown that only a session with the claim sees. The gate is identity: the admin server
  refuses every request whose JWT lacks `superadmin: true`. Separate ServiceAccount whose
  ClusterRole is the ONLY one with `create/patch/delete` on `Quota`, `QuotaRequest`, `Region`.
  The user server's role keeps `get/list` on those (it enforces limits and validates a region on
  create) plus `create` on `QuotaRequest` (a person or team admin opens one).
- The web's `/superadmin` area calls the admin api; `NEXT_PUBLIC_ADMIN_API_URL` (or the server-side
  equivalent) names it. Everything else in the web keeps calling `/v1`.
- `RUSTIC_GIT_WORKSPACES_ADMINS` bootstrap runs on the admin server only, and DEFAULTS to
  `karthik@kloudlite.io` when unset (owner, 2026-09-04), so a fresh deployment always has one
  superadmin who can add the rest from the admin area.

Not doing: a separate crate or binary (same code, twice the build); a second JWT secret (the
claim is the gate, and the admin server additionally refuses any token without it).

## Rules

- **Quotas are data in the cluster, decisions are people.** No automatic approval, no automatic
  purge when over quota; over-quota only blocks new allocation.
- **Usage is computed from the truth (CRDs), never cached.** A stale counter would let someone
  under-count; a list is cheap at this scale and already indexed by the owner label.
- **A team's quota is the team's, a person's is the person's.** Working copies owned by the team
  slug count against the team only.
- **Superadmin is a claim, not an owner.** It never changes who owns anything.
- **Admin writes only happen on the admin server.** RBAC, not convention, is what stops the user
  server writing a `Quota`.
- **Detached volumes count.** Disk kept by snapshots after a working copy is deleted is still the
  owner's disk; deleting snapshots is how they get it back.

## Cases

| case | behaviour |
|---|---|
| person at 5/5 workspaces creates one more | 409 "workspaces: 5 of 5 in use; request more under Quota" |
| team member creates a team workspace, team at limit | 409, same shape, team's numbers |
| team member (not admin) opens a request for the team | 403 "only a team admin can request a team quota" |
| team admin opens a second request while one is pending | 409 "a request is already pending" |
| superadmin approves | `Quota` written, request `approved`, the next create succeeds |
| restore of a snapshot when the owner is at the workspaces limit | 409; the snapshot stays |
| push when at the snapshots limit | 409; the working copy keeps running |
| owner with no `Quota` object | the `default-user` or `default-team` limits apply |
| superadmin lists another owner's workspaces | allowed (claim), audit-logged with the caller |
| `RUSTIC_GIT_WORKSPACES_ADMINS` set at boot | those users are ensured in the superadmin list; the env is otherwise unused |

## Not doing

Per-region quotas; billing; automatic scaling of limits; quota on git repositories or images
(server tier, separate).

## Testing

`/v1` recorder tests for every 409, the role check, the one-pending rule, approve/deny, the claim
arms; agent test for the `ResourceQuota` write on namespace ensure; web `bun:test` for the usage
bar math and request form validation; live: hit a limit, request, approve as superadmin, succeed.

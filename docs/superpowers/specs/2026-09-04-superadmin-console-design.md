# Superadmin console: product requirements

**Date:** 2026-09-04
**Status:** approved by the owner 2026-09-04 (drain and decommission from the console in phase 1; audit retention deferred, keep everything for now; approve-with-edits assumed). Supersedes the admin-area paragraphs of
`2026-09-03-quotas-and-superadmin-design.md` §4 and `2026-09-03-live-settings-design.md` §5–§7 as
far as the web is concerned; every backend route those specs added stays and is reused here.

## Who it is for

One persona: the **operator** — the owner and, later, one or two people they trust with the
superadmin claim. They run the platform for everyone else. They are technical, they are not in
this console all day, and when they open it something usually needs a decision or is wrong.

Jobs the operator comes here to do, in order of frequency:

1. **Decide a request.** Someone asked for more quota; approve or deny with a reason, in under a
   minute, with the facts needed to decide on the same screen.
2. **Answer "is it healthy?"** in ten seconds: the two clusters, every workload, the nodes, and
   anything that fired.
3. **Find one owner** and see everything about them: usage, limits, live working copies, requests,
   history.
4. **Run a change safely**: roll a workload, drain a node, add a region — with a preview of what
   will happen, a second look for the dangerous ones, and a record afterwards.
5. **Find out what happened**: who changed what, when, from where.

What it is NOT: a user-facing feature (users never see quotas, limits or this area), a settings
editor for every tunable (the owner ruled those stay in config), or a Kubernetes dashboard.

## Principles

- **Decisions first.** The landing view is what needs attention, not a menu.
- **Every page answers one question**, has one primary action, and shows the facts that action
  needs without a second click.
- **Dangerous is loud, routine is quiet.** Approve, roll, drain and delete each state what will
  happen before it happens; the server StatefulSet and node drains take a second confirmation.
- **Everything is auditable.** Every write records who, when, what and why; the Audit page shows
  it; nothing writes silently.
- **Truth from the cluster.** Numbers are computed from CRDs and pods on every render, never
  cached counters (the rule the whole product follows).
- **Same chrome as the product.** Tokens, `--radius: 0`, sibling components; it must look like the
  app, only denser.

## Information architecture

Top-level shell place `/superadmin` (reached from the profile menu, gated by the claim), with a
left rail on desktop and a tab row on narrow screens — the rail because there are seven areas and
the product's top tab row is for the org level.

| area | route | question it answers | primary action |
|---|---|---|---|
| **Overview** | `/superadmin` | What needs my attention now? | open the item |
| **Requests** | `/superadmin/requests` | Who asked for more, and should they get it? | approve / deny |
| **Owners** | `/superadmin/owners`, `/superadmin/owners/{slug}` | What does this person or team have and use? | set quota, open their objects |
| **Clusters** | `/superadmin/clusters`, `/superadmin/clusters/{region}` | Is this region healthy; what runs where? | drain node, roll agent/gateway, add region |
| **Monitoring** | `/superadmin/monitoring` | Are the central services healthy; what fired? | roll a workload |
| **Audit** | `/superadmin/audit` | What happened, by whom? | filter, export |
| **Access** | `/superadmin/access` | Who else is a superadmin? | add / remove |
| **Configuration** | `/superadmin/configuration` | What is the platform configured with? | read only (link to the deploy manifest) |

Quota defaults live under Owners (a "Defaults" card at the top of the list), not as their own
area: they are a property of owners.

Navigation (owner, 2026-09-04): rail → list → detail everywhere. The Owners LIST is the way into
any person or team (search box, usage against limits, tightest first, a pending-request badge);
the owner detail's "Open as <owner>" opens that owner's normal product pages. The Clusters LIST is
the way into a region. The header search inside superadmin is scoped to owners, nodes and
requests. The kloudlite logo returns to the product.

## Screens

### Overview

Cards, each a count with the top three items and a link:

- Pending requests (oldest first, with age).
- Attention: workloads not fully ready, nodes NotReady or draining, a region with zero agents,
  a cluster whose settings object failed to parse, alerts (see Monitoring) currently firing.
- Recent activity: the last ten audit entries.
- Fleet numbers: owners, live workspaces, live environments, snapshots, total disk allocated,
  per region.

Empty state when nothing needs attention: one sentence and the fleet numbers.

### Requests

A queue table: owner (person/team badge), what they asked for shown as **current → requested**
per dimension (only the dimensions they changed), their reason, current usage against the
current limit for those dimensions (so "at 5 of 5" is visible without leaving), age, requester.

Row action opens a decision panel: the same facts plus the owner's last three decided requests,
then **Approve** (applies exactly the requested values; an editable copy lets the operator grant
less or more before approving) or **Deny** with a required note. Both land in Audit. Decided
requests move to a "Decided" tab with who/when/note. Filters: person/team, dimension, age.

States: empty ("No pending requests"), loading, and the 409 when a request was decided by
someone else meanwhile.

### Owners

List: every owner with a Quota object or any live object, sortable by usage ratio of the tightest
dimension; columns: owner, kind, workspaces used/limit, environments, snapshots, disk, cpu,
memory, pending request badge. Search by slug. A "Defaults" card above the list edits
`default-user` and `default-team` (each field with its unit and the fleet's current max usage
as a hint, so a default below what someone already uses is called out).

Detail (`/owners/{slug}`): the six dimensions as bars (used, limit, source: own quota / default);
**Set quota** (edits the owner's own Quota; creating it from the default if absent; note
required); the owner's live workspaces and environments (state, node, region, age, with stop and
delete that go through the admin routes and are audited); their snapshots and volumes (detached
ones flagged, with disk); their request history; their audit trail.

### Clusters

List: one card per region: name, status (active/inactive), agents ready/desired, nodes
ready/total, draining nodes, live working copies, disk pool use if the agent reports it, the
region's settings object status (present / absent / failed to parse, observedGeneration lag).
**Add region** (id, display name) and activate/deactivate.

Detail (`/clusters/{region}`): nodes table (name, ready, agent pod ready, decommission status
with the draining counters `running= owned= copies= thin=`, working copies hosted, replicas
held); **Drain** (sets the decommission label; second confirmation; the sticky `drained`
timestamp shown when done) and **Undrain**; per-region workloads (agent DaemonSet, gateway) with
image, ready/desired, rollout state, last roll, **Roll** with reason; the cluster's settings
object as a read-only summary (which fields are stored vs env vs default) with a link to the
Configuration page — no editing, per the owner's rule.

### Monitoring

Central workloads table (server StatefulSet, api, admin, worker, gateway, web): image tag and
digest, ready/desired, rollout state, restarts in the last hour, last roll (who/when/reason),
health version from `/healthz`, **Roll** with reason (second confirmation for the server).

Signals, without Prometheus: the admin server scrapes each pod's `/metrics` on request (they are
in-cluster and already annotated for scraping) and evaluates the alert catalogue in
`deploy/alerts.md` where a single scrape can (leader count, fence detections, 5xx ratio over the
last window it can compute from two scrapes, reconcile error ratio, open tunnels). Each rule is a
row: firing / ok / unknown (needs a metric we cannot scrape, like node-exporter), with the
"why" from the catalogue. A link to Grafana appears only if `KLOUDLITE_GIT_GRAFANA_URL` is set.

Later (not in this plan): deploy Prometheus and read alerts from Alertmanager instead of scraping.

### Audit

Every admin write, one row: when, who (superadmin email), action (approve, deny, set quota,
roll, drain, add region, add/remove superadmin, settings change), target, reason/note, result.
Source: the annotations the routes already write on the objects (rolled-by, decided-by,
updated-by) UNIONED with a new append-only audit log the admin server writes to the object store
(`audit/{yyyy-mm}/{ts}-{id}.json`), so events survive object deletion. Filters by actor, action,
target, date; export as CSV.

### Access

Superadmin list (email, added by, added at); **Add** by email (must be an existing user), **Remove**
(cannot remove yourself; cannot remove the last one). The bootstrap email is shown as such.

### Configuration

Read-only: the effective tunables per scope (central, each cluster) with their source (stored /
env / built-in default) — the schema endpoint already provides this — and the image pins,
replicas and hosts. No editing; a sentence says where each is changed (deploy manifest, or the
admin API for the stored ones). This satisfies "monitoring, etc." without reopening the settings
editor the owner ruled out.

## Cross-cutting

- **Permissions**: everything behind the superadmin claim (server-side check on every page and
  every action; the admin API refuses without it before routing).
- **Reason on every write** except approve (the request carries its reason). Deny, set quota,
  roll, drain, add/remove superadmin: required note, stored.
- **Confirmation**: one for any write; a second, naming the consequence, for: server StatefulSet
  roll (database ownership moves), node drain (running work keeps running but nothing new lands
  there), removing a superadmin, deactivating a region.
- **Freshness**: pages poll every 10 s while open (the app's existing auto-refresh), faster (2 s)
  while a roll or drain is in progress.
- **Errors**: 409 shows the conflicting state (someone else decided, a roll still in progress with
  ready/desired); 422 shows the field and range; 5xx shows a retry with the request id.
- **Density and layout**: rail + content; tables with `tabular-nums`, sticky header, row actions
  on hover/focus, keyboard reachable; empty states in one sentence; no cards for their own sake.
- **Vocabulary**: workspace, environment, push, snapshot, restore, clone, delete; owner, person,
  team, region, node, workload, roll, drain.

## Backend gaps this needs (everything else already exists)

1. Requests: decided-request history per owner (list already exists; add `?owner=&state=`).
2. Owners: a per-owner detail endpoint (usage + limit + source + objects) — composes existing
   calls; a "set quota" route (`PUT /admin/quota/{owner}` exists).
3. Clusters: activate/deactivate region (status field exists); drain/undrain = set/unset the
   decommission label on the Node through the admin server (new, RBAC `nodes: patch` for the
   admin SA on k3s, name-restricted is impossible for nodes — scoped by label selector in the
   handler); per-node hosted counts from the agents' status (exists on Volume/Workspace CRs).
4. Monitoring: scrape endpoint on the admin server (`GET /admin/monitoring/signals`) evaluating
   the catalogue; restarts from pod status.
5. Audit: the append-only log writer in the admin server + `GET /admin/audit` with filters; every
   existing write route calls it.
6. Access: list/add/remove already exist on the server tier; the "not yourself / not the last"
   rules are new.
7. Overview: `GET /admin/overview` composing the above (one round trip for the landing page).

## Phasing

- **Phase 1 (this plan)**: Overview, Requests, Owners (list + detail + defaults), Monitoring
  (workloads + scraped signals), Audit (log + page), Access rules, Configuration (read-only),
  Clusters (list, detail, roll, add region). Drain/undrain included.
- **Phase 2 (later, separate spec)**: Prometheus + Alertmanager, notifications (email/Slack on a
  new request or a firing alert), scheduled reports.

## Not doing

Editing tunables in the UI; a general Kubernetes object browser; impersonation; per-user
notifications; anything users can see.

## Decisions (owner, 2026-09-04)

1. Approve with edits: the decision panel's editable copy lets the operator grant less or more
   than asked (assumed; "approve exactly as asked" is the fallback if the owner objects).
2. Node drain AND decommission from the console, phase 1: Drain sets the decommission label and
   shows the draining counters until the sticky `drained` timestamp; Decommission is the step
   after `drained` — a second confirmation, then the node is cordoned and the operator is told
   the VM may be deleted (the console never deletes the VM); Undrain removes the label.
3. Audit retention: keep everything; pruning is a later decision.

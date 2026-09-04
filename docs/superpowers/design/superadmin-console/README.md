Canvas: https://claude.ai/code/artifact/3fc67b69-fa38-46b5-ad22-1c8c688e4b16 (v2, approved 2026-09-04; full-width layout).

# Superadmin console — v2 artboards

Ten static `.dc.html` artboards, 1440 px wide, laid out on `canvas.json` in two rows of five.
Chrome, tokens, fonts and rail are carried over verbatim from the v1 set: Mona Sans / Hubot Sans /
JetBrains Mono, `#FAFAFA` ground, `#FFFFFF` cards, `#E4E4E7` borders, `#1E6FE8` brand, `#09090B`
ink, radius 0, 56 px header, 208 px rail with 32 px items. What v2 changes is craft, not palette.

The one structural move behind every page: **every content block is a section with the same
anatomy** — an 11 px uppercase eyebrow, a 13 px 600 title, an optional count chip, and a
right-aligned toolbar, over a 16 px padded body. v1's bare tables and unlabelled bordered boxes are
gone. Tables gained an uppercase sticky header, 40 px rows, `#F4F4F5` hover, right-aligned tabular
numbers, a status pill column, a sorted-column arrow, and row actions that appear at the right on
hover. Capacity is never a bare number: it is a 6 px bar (blue, amber at ≥ 80 %, red at 100 %) with
`used / limit unit` right-aligned under it.

**Main.dc.html** (1440×1320) — Overview. A five-tile KPI strip (pending requests, firing signals,
owners over 80 %, live workspaces, live environments), each with a 7-day inline-SVG sparkline; then
a two-column body: a "Needs attention" feed whose rows lead with a severity pill and carry
region/owner and age, beside "Capacity by region" with node-ready dots and disk/CPU/memory bars per
region. Below, a "Recent activity" timeline on a left time rail with actor initials, and a
"Requests waiting" mini table. Fixes v1's overview, which was three unlabelled cards and a
one-row table that answered nothing.

**Requests.dc.html** (1440×1240) — the generic queue. Requests are no longer quota-only: **quota,
access, region and other** all flow through one table with a kind pill, an owner (name and team pill
on one line), and a single Request column: the summary on one 13 px line that ellipses rather than
wraps, with the kind-specific context as a 12 px muted second line under it — a 120 px capacity bar
beside "19 / 20 in use" for quota, the current role for access, the current regions for region, the
first line of the ticket for other. Rows are a fixed 56 px, the table is `table-layout:fixed` so
nothing reflows, and Open · Deny are always rendered in muted grey, turning brand blue on the
hovered row. There is no Status column on the **Open** tab, because every row on it is open by
definition; the **Decided** tab restores that column and carries an approved / denied pill per row.
A 420 px
decision panel holds the selected row: a facts block, the requester's note, the owner's last three
decisions, and Approve (with the editable value inline) / Deny, both requiring a note. Fixes v1's
assumption that a request is always a quota raise.

**Owners.dc.html** (1440×1260) — a five-tile KPI strip, then the defaults expressed as ONE
comparison table (dimension rows with units, `default-user` and `default-team` as columns, Edit in
the toolbar) instead of v1's two separate lists, then the owners table sorted by the tightest
dimension so the owner about to hit a wall is at the top, with pending-request pills and hover
actions.

**Owner.dc.html** (1440×1620) — the owner detail. Breadcrumb, kind pill, "Open as acme" secondary
and "Set quota" primary; a KPI strip; the six quota dimensions as a 3×2 grid of capacity bars each
chipped `own` or `default` so the source of the limit is visible; then live workspaces and
environments, volumes and snapshots (detached and thin flagged), the owner's request history with
decisions, and an audit timeline. Fixes v1's flat six-bar block that never said where a limit came
from.

**Clusters.dc.html** (1440×980) — a KPI strip, then one card per region: status pill, per-node
ready dots, agents, disk-pool bar, live working copies, agent image tag and a ClusterSettings sync
chip, with Open. Below, regions-enabled-per-owner. Fixes v1's region rows that carried no capacity
signal at all.

**Cluster.dc.html** (1440×1180) — region detail with Roll agent / Roll gateway. The nodes table
shows the decommission status string verbatim (`draining running=2 owned=6 copies=4 thin=2`, and
the sticky `drained <RFC 3339>` that gates deleting the VM) with Drain / Undrain / Decommission on
hover; a workloads table with mono image tags, ready/desired and rollout pills; and a settings
summary as a key/value grid with source chips linking to Configuration. Fixes v1, which showed node
names and nothing operable.

**Monitoring.dc.html** (1440×1180) — KPI strip, the alert rules from `deploy/alerts.md` as a
signals table (rule, state pill including `unknown`, why it fires, the current detail, last change),
central workloads with restarts and last roll, and an "Active silences" section that is an honest
empty state — one sentence plus one action — rather than an empty box. Grafana sits in the page
header.

**Audit.dc.html** (1440×980) — new in v2. KPI strip (events today, actors, refusals, exports), a
filter toolbar (actor, free-text action, date range, Export CSV) and the log itself: when, actor with
avatar, action pill, mono target, note, and a result pill that distinguishes `ok` from `error: 409`
— so a refused create and an admission denial read as first-class events, not absences.

**Access.dc.html** (1440×960) — new in v2. Superadmins with who added them, when, a `bootstrap`
origin chip and last seen, Remove on hover; the Add form as a proper section with email, a required
note and one primary; and the removal confirmation panel showing the consequence sentence (the claim
dies at next sign-in, their pending approvals stay pending) with a required note above a danger
outline button.

**Configuration.dc.html** (1440×1620) — new in v2, read-only. A three-tile row explains **where**
things are configured — deploy manifest, environment, stored — then one section per scope (central,
and each cluster) as a table of Field, current value, source chip (`stored` / `env` / `default`),
range, and takes-effect (`live`, or `boot: rolls <reader>`). A search box filters fields. Nothing is
editable here, which is the point: the page exists so nobody has to guess whether a value came from
the manifest, the environment or the admin API.

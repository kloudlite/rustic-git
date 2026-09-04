import type {
  AdminClusterDetail,
  AdminClusterRow,
  ApiEnvironment,
  ApiRegion,
  ApiVolumeSummary,
  ApiWorkspace,
  OwnerDetail,
  OwnerRow,
  Overview,
  QuotaRequestDoc,
  RequestDoc,
  SettingsSchema,
  SignalsResponse,
  SuperAdmin,
  WorkloadDoc,
} from "@/lib/api";
import type { AuditEntry, AuditPage } from "@/lib/audit";
import type { HistoryEvent, HistorySeries } from "@/lib/history";
import type { QuotaDim, QuotaReport } from "@/lib/quota";

/** Offline seed for `RUSTIC_GIT_ADMIN_FIXTURES=1`.
 *
 *  The console's gate is "every screen renders with realistic data", and it is checked by
 *  screenshot before merge — which needs a fleet nobody has on a laptop. So the numbers here are
 *  internally consistent rather than decorative: the owners' usage matches the fleet totals on
 *  the overview, the regions here are the regions the clusters list names, and the owners span
 *  calm, warn and critical so every tone appears on one screen.
 *
 *  Typed against the wire types on purpose — a field rename must break `bunx tsc`, not a
 *  screenshot nobody reads. */

const DAY = 86_400_000;
/** Every timestamp is relative to when the process started, not a frozen date: a seed pinned to a
 *  calendar day renders as "in 6 hours" the moment the machine clock passes it, and a screenshot
 *  full of future events reads as a bug in the console rather than as a stale fixture. */
const now = Date.now();
const ago = (hours: number) => new Date(now - hours * 3_600_000).toISOString();
/** Seven daily points ending "now", so a sparkline has the same 7-day shape every page assumes. */
function series(values: number[]): Omit<HistorySeries, "available"> {
  const end = now;
  return {
    series: values.map((value, i) => ({ ts: new Date(end - (values.length - 1 - i) * DAY).toISOString(), value })),
    summary: {
      last: values[values.length - 1],
      delta: Math.round((values[values.length - 1] - values[0]) * 100) / 100,
      min: Math.min(...values),
      max: Math.max(...values),
    },
  };
}

const quota = (n: [number, number, number, number, number, number]): Record<QuotaDim, number> => ({
  workspaces: n[0],
  environments: n[1],
  snapshots: n[2],
  diskGb: n[3],
  cpu: n[4],
  memoryGb: n[5],
});

const TEAM_LIMIT = quota([20, 8, 80, 400, 32, 128]);
const USER_LIMIT = quota([3, 1, 20, 100, 8, 16]);

const OWNERS: OwnerRow[] = [
  // acme is over its ceiling on workspaces — the critical bar, and the reason a request waits.
  { owner: "acme", isTeam: true, limit: TEAM_LIMIT, used: quota([20, 6, 64, 310, 22, 96]), source: "own", pending: true },
  // ops-lab sits in the warn band, near its cpu ceiling but not at it.
  { owner: "ops-lab", isTeam: true, limit: TEAM_LIMIT, used: quota([17, 4, 38, 180, 28, 74]), source: "own", pending: true },
  { owner: "priya", isTeam: false, limit: USER_LIMIT, used: quota([1, 0, 12, 52, 5, 9]), source: "default", pending: false },
];

const REQUESTS: QuotaRequestDoc[] = [
  {
    id: "qr-acme-ws",
    owner: "acme",
    requested: { workspaces: 40, diskGb: 600 },
    reason: "Six contractors start Monday and each needs a workspace.",
    state: "pending",
    createdAt: ago(53),
    decidedBy: null,
    decidedAt: null,
    note: null,
  },
  {
    id: "qr-opslab-cpu",
    owner: "ops-lab",
    requested: { cpu: 48, memoryGb: 160 },
    reason: "The EU load tests peg every core we have.",
    state: "pending",
    createdAt: ago(18),
    decidedBy: null,
    decidedAt: null,
    note: null,
  },
  {
    id: "qr-priya-snap",
    owner: "priya",
    requested: { snapshots: 40 },
    reason: "Keeping a push per release candidate.",
    state: "approved",
    createdAt: ago(140),
    decidedBy: "karthik",
    decidedAt: ago(139),
    note: "Cheap; snapshots are refcounted.",
  },
];

/** The generic queue behind `/admin/requests` — all four kinds, so the Requests screen shows one
 *  of each rather than four quota rows. Deliberately a superset of `REQUESTS` in shape, not in
 *  identity: the two endpoints are separate CRDs and the console reads them separately. */
const GENERIC_REQUESTS: RequestDoc[] = [
  {
    id: "rq-acme-ws",
    owner: "acme",
    kind: "quota",
    requestedBy: "meera",
    reason:
      "We are onboarding six contractors on Monday and every one of them needs their own workspace. The old ones are pushed and stopped — deleting them would lose their snapshots.",
    quota: { workspaces: 40 },
    state: "pending",
    createdAt: ago(53),
  },
  {
    id: "rq-rahul-admin",
    owner: "rahul",
    kind: "access",
    requestedBy: "rahul",
    reason: "I run the release rota now and need to invite the new joiners myself.",
    access: { team: "acme", role: "admin" },
    state: "pending",
    createdAt: ago(27),
  },
  {
    id: "rq-opslab-eu",
    owner: "ops-lab",
    kind: "region",
    requestedBy: "vikram",
    reason: "The EU load tests have to run next to the customers they simulate.",
    region: { region: "westeurope-k3s" },
    state: "pending",
    createdAt: ago(19),
  },
  {
    id: "rq-priya-disk",
    owner: "priya",
    kind: "quota",
    requestedBy: "priya",
    reason: "Keeping a push per release candidate, and each one is about 8 GB.",
    quota: { diskGb: 250 },
    state: "pending",
    createdAt: ago(6),
  },
  {
    id: "rq-sana-snap",
    owner: "sana",
    kind: "other",
    requestedBy: "sana",
    reason: "It was the only copy of the migration I had.",
    other: { title: "Restore a snapshot deleted by mistake", body: "deleted snap-4c1e at 09:40 while cleaning up\nthe volume is vol-sana-2" },
    state: "pending",
    createdAt: ago(0.7),
  },
  {
    id: "rq-acme-snap",
    owner: "acme",
    kind: "quota",
    requestedBy: "meera",
    reason: "One push per release candidate.",
    quota: { snapshots: 120 },
    state: "approved",
    decidedBy: "karthik",
    decidedAt: ago(140),
    note: "Cheap; snapshots are refcounted.",
    createdAt: ago(146),
  },
  {
    id: "rq-acme-cpu",
    owner: "acme",
    kind: "quota",
    requestedBy: "meera",
    reason: "The build fleet is queueing.",
    quota: { cpu: 96 },
    state: "denied",
    decidedBy: "karthik",
    decidedAt: ago(700),
    note: "Ask again once the idle workspaces are stopped.",
    createdAt: ago(710),
  },
];

const AUDIT: AuditEntry[] = [
  { ts: ago(2), actor: "karthik", action: "quota.set", target: "Quota/priya", reason: "Approved qr-priya-snap.", result: "ok" },
  { ts: ago(4), actor: "karthik", action: "workload.roll", target: "Deployment/rustic-git-api", reason: "Rotate the peer secret before the drain.", result: "ok" },
  { ts: ago(8), actor: "karthik", action: "node.drain", target: "session-3", reason: "Host is being resized to D16s v5.", result: "ok" },
  { ts: ago(22), actor: "meera", action: "workspace.stop", target: "ws-acme-checkout", reason: "Idle for six days and holding 40 GiB.", result: "ok" },
  { ts: ago(26), actor: "karthik", action: "quota.set", target: "Quota/acme", reason: "Interim raise while qr-acme-ws is decided.", result: "ok" },
  { ts: ago(28), actor: "karthik", action: "environment.stop", target: "env-acme-preview", reason: "Nothing has connected to it in a week.", result: "ok" },
  { ts: ago(32), actor: "karthik", action: "region.status", target: "westeurope-k3s", reason: "Bringing the EU region up for the load tests.", result: "ok" },
  { ts: ago(48), actor: "karthik", action: "superadmin.add", target: "meera@kloudlite.io", reason: "Second pair of hands for the on-call rota.", result: "ok" },
  { ts: ago(54), actor: "meera", action: "quota.request.deny", target: "qr-sana-disk", reason: "Ask again once the old snapshots are deleted.", result: "ok" },
  { ts: ago(79), actor: "karthik", action: "environment.delete", target: "env-opslab-staging", reason: "Superseded by env-opslab-staging-2.", result: "ok" },
];

const EVENTS: HistoryEvent[] = [
  { id: "e1", ts: ago(2), kind: "request.approved", actor: "karthik", owner: "priya", target: "Quota/priya", region: null, attrs: { detail: "snapshots 20 → 40", note: "Cheap; snapshots are refcounted." } },
  { id: "e2", ts: ago(4), kind: "workload.roll", actor: "karthik", owner: null, target: "Deployment/rustic-git-api", region: "centralindia-k3s", attrs: { note: "Rotate the peer secret before the drain." } },
  { id: "e3", ts: ago(8), kind: "node.drain", actor: "karthik", owner: null, target: "session-3", region: "centralindia-k3s", attrs: { note: "Host is being resized to D16s v5." } },
  { id: "e4", ts: ago(22), kind: "workspace.stop", actor: "meera", owner: "acme", target: "ws-acme-checkout", region: "centralindia-k3s", attrs: { detail: "idle 6 days" } },
  { id: "e5", ts: ago(32), kind: "quota.set", actor: "karthik", owner: "ops-lab", target: "Quota/ops-lab", region: null, attrs: { detail: "cpu 24 → 32" } },
];

const WORKLOADS: WorkloadDoc[] = [
  { scope: "central", name: "rustic-git-srv", kind: "statefulset", image: "ghcr.io/kloudlite/rustic-git:f87fddb1", ready: 3, desired: 3, rolloutState: "Stable", lastRoll: null },
  { scope: "central", name: "rustic-git-api", kind: "deployment", image: "ghcr.io/kloudlite/rustic-git-api:f87fddb1", ready: 1, desired: 2, rolloutState: "RollingOut", lastRoll: { by: "karthik", at: ago(4), reason: "Rotate the peer secret before the drain." } },
  { scope: "central", name: "rustic-git-worker", kind: "deployment", image: "ghcr.io/kloudlite/rustic-git-worker:f87fddb1", ready: 2, desired: 2, rolloutState: "Stable", lastRoll: null },
  { scope: "central", name: "rustic-git-gateway", kind: "deployment", image: "ghcr.io/kloudlite/rustic-git-gateway:f87fddb1", ready: 2, desired: 2, rolloutState: "Stable", lastRoll: null },
  { scope: "centralindia-k3s", name: "rustic-git-agent", kind: "daemonset", image: "ghcr.io/kloudlite/rustic-git-agent:f87fddb1", ready: 3, desired: 3, rolloutState: "Stable", lastRoll: { by: "karthik", at: ago(122), reason: "Pick up the volume takeover build." } },
  { scope: "westeurope-k3s", name: "rustic-git-agent", kind: "daemonset", image: "ghcr.io/kloudlite/rustic-git-agent:5319f67d", ready: 1, desired: 2, rolloutState: "RollingOut", lastRoll: { by: "karthik", at: ago(5), reason: "Repin the EU region to the node-death build." } },
];

const CLUSTERS: AdminClusterRow[] = [
  { region: "centralindia-k3s", status: "active", agentsReady: 3, agentsDesired: 3, nodesReady: 3, nodesTotal: 3, draining: 1, workingCopies: 41, settingsStatus: "present" },
  // The EU region is mid-roll, so its agents lag the settings document — the "stale" tone.
  { region: "westeurope-k3s", status: "active", agentsReady: 1, agentsDesired: 2, nodesReady: 1, nodesTotal: 2, draining: 0, workingCopies: 12, settingsStatus: "stale (lag 1)" },
];

const CLUSTER_SETTINGS: Record<string, Record<string, unknown>> = {
  "centralindia-k3s": { syncSecs: 300, nodeDeadSecs: 180, decommissionSecs: 30, retainSyncPoints: 1, workspaceImage: "ghcr.io/kloudlite/kl-base:2026-08-21" },
  "westeurope-k3s": { syncSecs: 600, nodeDeadSecs: 180, decommissionSecs: 30, retainSyncPoints: 1, workspaceImage: "ghcr.io/kloudlite/kl-base:2026-08-21" },
};

const CLUSTER_DETAIL: Record<string, AdminClusterDetail> = {
  "centralindia-k3s": {
    region: "centralindia-k3s",
    status: "active",
    nodes: [
      { name: "session-1", ready: true, decommission: false, decommissionStatus: null, workingCopies: 18, replicasHeld: 21 },
      { name: "session-2", ready: true, decommission: false, decommissionStatus: null, workingCopies: 15, replicasHeld: 19 },
      { name: "session-3", ready: true, decommission: true, decommissionStatus: "draining running=2 owned=6 copies=4 thin=2", workingCopies: 8, replicasHeld: 6 },
    ],
    workloads: WORKLOADS.filter((w) => w.scope === "centralindia-k3s"),
    settings: CLUSTER_SETTINGS["centralindia-k3s"],
  },
  "westeurope-k3s": {
    region: "westeurope-k3s",
    status: "active",
    nodes: [
      { name: "eu-1", ready: true, decommission: false, decommissionStatus: null, workingCopies: 12, replicasHeld: 9 },
      // A node that is simply down: the dead-node sweep's own case, and the only "not ready" row.
      { name: "eu-2", ready: false, decommission: false, decommissionStatus: null, workingCopies: 0, replicasHeld: 7 },
    ],
    workloads: WORKLOADS.filter((w) => w.scope === "westeurope-k3s"),
    settings: CLUSTER_SETTINGS["westeurope-k3s"],
  },
};

const SIGNALS: SignalsResponse = {
  // The catalogue is `deploy/alerts.md`'s, verbatim, including the two this process cannot
  // evaluate without a window — `unknown` is a first-class answer there, never guessed as ok.
  signals: [
    { alert: "NoLeader", state: "ok", why: "Exactly one pod holds the lease.", detail: "ownership_is_leader = 1", region: null },
    { alert: "LeaseRenewFailing", state: "ok", why: "No renew failures in the last scrape.", detail: "0 failures across 3 pods", region: null },
    { alert: "DbFenceDetected", state: "ok", why: "No node has opened a database another holds.", detail: "db_fence_detected_total = 0", region: null },
    { alert: "Http5xxRate", state: "ok", why: "Below the 5% threshold on both listeners.", detail: "public 0.4%, peer 0.1%", region: null },
    { alert: "MisdirectedWrites", state: "firing", why: "421s have not settled since the api roll.", detail: "0.34/s over 10m, threshold 0.1/s", region: null },
    { alert: "ReconcileErrors", state: "ok", why: "Under the 20% error ratio for every kind.", detail: "workspace 0%, volume 2%, environment 0%", region: "centralindia-k3s" },
    { alert: "TunnelSaturation", state: "ok", why: "Well under MAX_TUNNELS.", detail: "max 214 of 1000 per pod", region: "centralindia-k3s" },
    { alert: "WorkerHeartbeatStale", state: "unknown", why: "Needs a 1 h restart window this process cannot see.", detail: null, region: "centralindia-k3s" },
    { alert: "PoolAlmostFull", state: "firing", why: "btrfs starts failing allocations past 80%.", detail: "session-3 at 84% of /wspool-prod", region: "centralindia-k3s" },
    { alert: "NodeDiskAlmostFull", state: "unknown", why: "node-exporter was not scrapeable on eu-2.", detail: null, region: "westeurope-k3s" },
  ],
  restarts: [
    { workload: "rustic-git-srv", restarts: 0 },
    { workload: "rustic-git-api", restarts: 3 },
    { workload: "rustic-git-worker", restarts: 1 },
    { workload: "rustic-git-gateway", restarts: 0 },
  ],
  scrape_failures: [["eu-2", "connect timed out after 2s"]],
  pods_listed: 17,
  hyperdx_url: "https://hyperdx.kloudlite.io/search",
};

const OVERVIEW: Overview = {
  pendingRequests: REQUESTS.filter((r) => r.state === "pending"),
  attention: [
    { kind: "signal.firing", detail: "MisdirectedWrites has been over threshold for 14 minutes", href: "/superadmin/monitoring" },
    { kind: "critical", detail: "acme is at its workspace ceiling — 20 of 20 in use", href: "/superadmin/owners/acme" },
    { kind: "not_ready", detail: "eu-2 has been not ready for 22 minutes", href: "/superadmin/clusters/westeurope-k3s" },
    { kind: "draining", detail: "session-3 is draining — 2 running, 4 copies left", href: "/superadmin/clusters/centralindia-k3s" },
    { kind: "rolling", detail: "rustic-git-api is 1 of 2 ready", href: "/superadmin/monitoring" },
  ],
  recentAudit: AUDIT.slice(0, 5),
  fleet: {
    owners: 3,
    workspaces: 53,
    environments: 11,
    snapshots: 114,
    diskGbTotal: 542,
    perRegion: {
      "centralindia-k3s": { owners: 3, workspaces: 41, environments: 8, snapshots: 88, diskGb: 410 },
      "westeurope-k3s": { owners: 2, workspaces: 12, environments: 3, snapshots: 26, diskGb: 132 },
    },
  },
};

const WORKSPACES: ApiWorkspace[] = [
  { id: "ws-acme-checkout", owner: "acme", team: "acme", name: "checkout", region: "centralindia-k3s", state: "ready", image: "ghcr.io/kloudlite/kl-base:2026-08-21", placement: "session-1", volume: "vol-acme-checkout", quota_gb: 40, packages: ["nodejs_22", "pnpm"] },
  { id: "ws-acme-billing", owner: "acme", team: "acme", name: "billing", region: "centralindia-k3s", state: "stopped", image: "ghcr.io/kloudlite/kl-base:2026-08-21", placement: "session-2", volume: "vol-acme-billing", quota_gb: 30, packages: ["rustc", "cargo"], replicated: { ready: true, reason: "Replicated", message: "another node holds the final sync point" } },
  { id: "ws-acme-search", owner: "acme", team: "acme", name: "search", region: "centralindia-k3s", state: "ready", image: "ghcr.io/kloudlite/kl-base:2026-08-21", placement: "session-3", volume: "vol-acme-search", quota_gb: 60, packages: ["go", "protobuf"], decommissioning: { ready: true, reason: "NodeLeaving", message: "the node is being retired; the next start lands elsewhere" } },
];

const ENVIRONMENTS: ApiEnvironment[] = [
  { id: "env-acme-staging", owner: "acme", name: "staging", region: "centralindia-k3s", state: "running", placement: "session-1", volume: "vol-acme-staging", services: [
    { name: "api", image: "ghcr.io/acme/api:2026-09-01", command: [], env: { MONGO_URL: "mongodb://db:27017" }, mounts: [], ports: [8080] },
    { name: "db", image: "mongo:7", command: [], env: {}, mounts: [{ folder: "data", path: "/data/db" }], ports: [27017] },
  ] },
  { id: "env-acme-preview", owner: "acme", name: "preview", region: "centralindia-k3s", state: "stopped", placement: "session-2", volume: "vol-acme-preview", services: [
    { name: "web", image: "ghcr.io/acme/web:2026-08-28", command: [], env: {}, mounts: [], ports: [3000] },
  ], replicated: { ready: false, reason: "AwaitingReplica", message: "no other node holds the final sync point yet" } },
];

const VOLUMES: ApiVolumeSummary[] = [
  { name: "vol-acme-checkout", kind: "workspace", volume: "vol-acme-checkout", display_name: "checkout", deleted: false, snapshots: 9, last_push_at: ago(3) },
  { name: "vol-acme-billing", kind: "workspace", volume: "vol-acme-billing", display_name: "billing", deleted: false, snapshots: 4, last_push_at: ago(70) },
  // A volume outliving its workspace — the row that explains why disk is still in use.
  { name: "vol-acme-legacy-import", kind: "workspace", volume: "vol-acme-legacy-import", display_name: "legacy-import", deleted: true, snapshots: 12, last_push_at: ago(1140) },
  { name: "vol-acme-staging", kind: "environment", volume: "vol-acme-staging", display_name: "staging", deleted: false, snapshots: 6, last_push_at: ago(21) },
];

/** One owner's detail page. Only `acme` carries live objects — the console links to a slug from a
 *  row, and a slug with no seed is answered from its row alone rather than invented. */
function ownerDetail(slug: string): OwnerDetail | undefined {
  const row = OWNERS.find((o) => o.owner === slug);
  if (!row) return undefined;
  const mine = slug === "acme";
  return {
    ...row,
    workspaces: mine ? WORKSPACES : [],
    environments: mine ? ENVIRONMENTS : [],
    volumes: mine ? VOLUMES : [],
    requests: REQUESTS.filter((r) => r.owner === slug),
    audit: AUDIT.filter((a) => a.target.includes(slug) || a.reason?.includes(slug)).slice(0, 10),
  };
}

const SCHEMA: SettingsSchema = {
  central: [
    { name: "maxBody", description: "Largest git push body the server accepts.", unit: "bytes", range: { min: 1_048_576, max: 4_294_967_296 }, mark: "live", readers: ["rustic-git-srv"], default: 2_147_483_648, env: "RUSTIC_GIT_MAX_BODY" },
    { name: "maxLayer", description: "Largest registry blob accepted in one request.", unit: "bytes", range: { min: 1_048_576, max: 5_368_709_120 }, mark: "live", readers: ["rustic-git-srv"], default: 5_368_709_120, env: "RUSTIC_GIT_MAX_LAYER" },
    { name: "mergeConcurrency", description: "Merges the worker runs at once per pod.", unit: "jobs", range: { min: 1, max: 16 }, mark: "live", readers: ["rustic-git-worker"], default: 4, env: "RUSTIC_GIT_MERGE_CONCURRENCY" },
    { name: "maxTunnels", description: "Open workspace tunnels per gateway pod.", unit: "tunnels", range: { min: 100, max: 5000 }, mark: "live", readers: ["rustic-git-gateway"], default: 1000, env: "RUSTIC_GIT_MAX_TUNNELS" },
    { name: "cloneHost", description: "Hostname handed out in clone URLs.", unit: "", range: null, mark: "boot", readers: ["rustic-git-srv", "rustic-git-api"], default: "git.kloudlite.io", env: "RUSTIC_GIT_CLONE_HOST" },
  ],
  cluster: [
    { name: "syncSecs", description: "How often a running worktree is cut a sync point.", unit: "seconds", range: { min: 30, max: 3600 }, mark: "live", readers: ["rustic-git-agent"], default: 300, env: "WS_SYNC_SECS" },
    { name: "nodeDeadSecs", description: "Silence after which a node is unplaceable.", unit: "seconds", range: { min: 60, max: 1800 }, mark: "live", readers: ["rustic-git-agent"], default: 180, env: "WS_NODE_DEAD_SECS" },
    { name: "decommissionSecs", description: "Drain beat on a node that is leaving.", unit: "seconds", range: { min: 10, max: 300 }, mark: "live", readers: ["rustic-git-agent"], default: 30, env: "WS_DECOMMISSION_SECS" },
    { name: "retainSyncPoints", description: "Ready sync points kept per worktree.", unit: "cuts", range: { min: 1, max: 5 }, mark: "live", readers: ["rustic-git-agent"], default: 1, env: null },
    { name: "workspaceImage", description: "Image a workspace runs unless it names its own.", unit: "", range: null, mark: "boot", readers: ["rustic-git-agent"], default: "ghcr.io/kloudlite/kl-base:2026-08-21", env: "WS_IMAGE" },
  ],
};

const CENTRAL_SETTINGS: Record<string, unknown> = {
  maxBody: 2_147_483_648,
  maxLayer: 5_368_709_120,
  mergeConcurrency: 6,
  maxTunnels: null,
  cloneHost: "git.kloudlite.io",
};

const SUPERADMINS: SuperAdmin[] = [
  { _id: "karthik@kloudlite.io", addedAt: "2026-01-14T06:00:00Z", addedBy: "bootstrap (RUSTIC_GIT_WORKSPACES_ADMINS)" },
  { _id: "meera@kloudlite.io", addedAt: ago(48), addedBy: "karthik@kloudlite.io" },
];

const REGIONS: ApiRegion[] = [
  { id: "centralindia-k3s", status: "active" },
  { id: "westeurope-k3s", status: "active" },
];

/** Ratios for the three node gauges, whole numbers for the counts — the shapes each tile expects
 *  (`regionCapacity` renders `pool_used` and friends as a percentage of 100). */
const SERIES: Record<string, number[]> = {
  pending_requests: [4, 5, 5, 6, 6, 7, 2],
  firing_signals: [0, 1, 1, 2, 2, 2, 2],
  owners_over_80: [1, 1, 2, 2, 2, 2, 2],
  live_workspaces: [45, 47, 48, 50, 51, 52, 53],
  live_environments: [9, 9, 10, 10, 11, 11, 11],
  decided_requests: [1, 3, 2, 0, 2, 2, 2],
  time_to_decide_p50: [9, 8, 8, 7, 6, 5, 5],
  restarts: [0, 0, 1, 1, 3, 4, 4],
  audit_events: [92, 118, 130, 141, 137, 152, 148],
  pool_used: [0.71, 0.73, 0.75, 0.77, 0.78, 0.79, 0.8],
  cpu_used: [0.6, 0.63, 0.65, 0.68, 0.7, 0.71, 0.72],
  memory_used: [0.7, 0.71, 0.73, 0.74, 0.75, 0.76, 0.76],
};

/** A region's own gauges differ from the fleet's, so a two-region page does not draw one line
 *  twice. The EU region runs colder — it was brought up for the load tests last week. */
const REGION_SCALE: Record<string, number> = { "westeurope-k3s": 0.55 };

const EXACT: Record<string, unknown> = {
  "/admin/overview": OVERVIEW,
  "/admin/owners": OWNERS,
  "/admin/clusters": CLUSTERS,
  "/admin/workloads": WORKLOADS,
  "/admin/monitoring/signals": SIGNALS,
  "/admin/settings/schema": SCHEMA,
  "/admin/settings/central": CENTRAL_SETTINGS,
  // Read through the ordinary api host, not /admin — but the same single funnel, so the same seed.
  "/v1/regions": REGIONS,
  "/api/admin/superadmins": SUPERADMINS,
};

const DEFAULT_QUOTA: Record<string, QuotaReport> = {
  "default-user": { owner: "default-user", limit: USER_LIMIT, used: quota([0, 0, 0, 0, 0, 0]) },
  "default-team": { owner: "default-team", limit: TEAM_LIMIT, used: quota([0, 0, 0, 0, 0, 0]) },
};

function auditPage(query: string): AuditPage {
  const q = new URLSearchParams(query);
  const actor = q.get("actor");
  const action = q.get("action");
  const target = q.get("target");
  // The filters the page sends are narrowing, not searching — same substring rule the api uses,
  // so a screenshot of a filtered table is not wider than a real one.
  const rows = AUDIT.filter(
    (r) =>
      (!actor || r.actor.includes(actor)) &&
      (!action || r.action.includes(action)) &&
      (!target || r.target.includes(target)),
  );
  return { rows, next_cursor: null };
}

/** `undefined` for anything unseeded, so the guard falls through to the real host rather than
 *  manufacturing an empty 200 that would render as a broken page — a blank section in a
 *  screenshot reads as "the console is broken", not as "the fixture is thin". */
export function fixtureFor(path: string): unknown | undefined {
  const [bare, query = ""] = path.split("?");

  if (bare === "/admin/history/events") {
    const limit = Number(new URLSearchParams(query).get("limit") ?? EVENTS.length);
    const owner = new URLSearchParams(query).get("owner");
    const rows = owner ? EVENTS.filter((e) => e.owner === owner) : EVENTS;
    return { events: rows.slice(0, limit), cursor: null };
  }
  if (bare.startsWith("/admin/history/")) {
    const values = SERIES[decodeURIComponent(bare.slice("/admin/history/".length))];
    if (!values) return undefined;
    const scale = REGION_SCALE[new URLSearchParams(query).get("region") ?? ""] ?? 1;
    return series(values.map((v) => Math.round(v * scale * 100) / 100));
  }
  if (bare === "/admin/requests") {
    const q = new URLSearchParams(query);
    const owner = q.get("owner");
    const kind = q.get("kind");
    const state = q.get("state");
    return GENERIC_REQUESTS.filter(
      (r) => (!owner || r.owner === owner) && (!kind || r.kind === kind) && (!state || r.state === state),
    );
  }
  if (bare === "/admin/quota-requests") {
    const q = new URLSearchParams(query);
    const owner = q.get("owner");
    const state = q.get("state");
    return REQUESTS.filter((r) => (!owner || r.owner === owner) && (!state || r.state === state));
  }
  if (bare === "/admin/audit") return auditPage(query);
  if (bare.startsWith("/admin/owners/")) return ownerDetail(decodeURIComponent(bare.slice("/admin/owners/".length)));
  if (bare.startsWith("/admin/clusters/")) return CLUSTER_DETAIL[decodeURIComponent(bare.slice("/admin/clusters/".length))];
  if (bare.startsWith("/admin/settings/clusters/")) {
    const spec = CLUSTER_SETTINGS[decodeURIComponent(bare.slice("/admin/settings/clusters/".length))];
    return spec ? { spec } : undefined;
  }
  if (bare === "/v1/quota") return DEFAULT_QUOTA[new URLSearchParams(query).get("owner") ?? ""];

  return EXACT[bare];
}

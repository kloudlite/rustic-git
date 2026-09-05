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
  SloJourney,
  SloJourneyStage,
  SloOverview,
  SloRun,
  SloRunDetail,
  SloStatus,
  SloStep,
  SuperAdmin,
  WorkloadDoc,
} from "@/lib/api";
import type { AuditEntry, AuditPage } from "@/lib/audit";
import type { HistoryEvent, HistorySeries } from "@/lib/history";
import type { QuotaDim, QuotaReport } from "@/lib/quota";

/** Offline seed for `KLOUDLITE_ADMIN_FIXTURES=1`.
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
  { ts: ago(4), actor: "karthik", action: "workload.roll", target: "Deployment/kloudlite-api", reason: "Rotate the peer secret before the drain.", result: "ok" },
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
  { id: "e2", ts: ago(4), kind: "workload.roll", actor: "karthik", owner: null, target: "Deployment/kloudlite-api", region: "centralindia-k3s", attrs: { note: "Rotate the peer secret before the drain." } },
  { id: "e3", ts: ago(8), kind: "node.drain", actor: "karthik", owner: null, target: "session-3", region: "centralindia-k3s", attrs: { note: "Host is being resized to D16s v5." } },
  { id: "e4", ts: ago(22), kind: "workspace.stop", actor: "meera", owner: "acme", target: "ws-acme-checkout", region: "centralindia-k3s", attrs: { detail: "idle 6 days" } },
  { id: "e5", ts: ago(32), kind: "quota.set", actor: "karthik", owner: "ops-lab", target: "Quota/ops-lab", region: null, attrs: { detail: "cpu 24 → 32" } },
];

const WORKLOADS: WorkloadDoc[] = [
  { scope: "central", name: "kloudlite-srv", kind: "statefulset", image: "ghcr.io/kloudlite/kloudlite:f87fddb1", ready: 3, desired: 3, rolloutState: "Stable", lastRoll: null },
  { scope: "central", name: "kloudlite-api", kind: "deployment", image: "ghcr.io/kloudlite/kloudlite-api:f87fddb1", ready: 1, desired: 2, rolloutState: "RollingOut", lastRoll: { by: "karthik", at: ago(4), reason: "Rotate the peer secret before the drain." } },
  { scope: "central", name: "kloudlite-worker", kind: "deployment", image: "ghcr.io/kloudlite/kloudlite-worker:f87fddb1", ready: 2, desired: 2, rolloutState: "Stable", lastRoll: null },
  { scope: "central", name: "kloudlite-gateway", kind: "deployment", image: "ghcr.io/kloudlite/kloudlite-gateway:f87fddb1", ready: 2, desired: 2, rolloutState: "Stable", lastRoll: null },
  { scope: "centralindia-k3s", name: "kloudlite-agent", kind: "daemonset", image: "ghcr.io/kloudlite/kloudlite-agent:f87fddb1", ready: 3, desired: 3, rolloutState: "Stable", lastRoll: { by: "karthik", at: ago(122), reason: "Pick up the volume takeover build." } },
  { scope: "westeurope-k3s", name: "kloudlite-agent", kind: "daemonset", image: "ghcr.io/kloudlite/kloudlite-agent:5319f67d", ready: 1, desired: 2, rolloutState: "RollingOut", lastRoll: { by: "karthik", at: ago(5), reason: "Repin the EU region to the node-death build." } },
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
    { workload: "kloudlite-srv", restarts: 0 },
    { workload: "kloudlite-api", restarts: 3 },
    { workload: "kloudlite-worker", restarts: 1 },
    { workload: "kloudlite-gateway", restarts: 0 },
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
    { kind: "rolling", detail: "kloudlite-api is 1 of 2 ready", href: "/superadmin/monitoring" },
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
    { name: "maxBody", description: "Largest git push body the server accepts.", unit: "bytes", range: { min: 1_048_576, max: 4_294_967_296 }, mark: "live", readers: ["kloudlite-srv"], default: 2_147_483_648, env: "KLOUDLITE_MAX_BODY" },
    { name: "maxLayer", description: "Largest registry blob accepted in one request.", unit: "bytes", range: { min: 1_048_576, max: 5_368_709_120 }, mark: "live", readers: ["kloudlite-srv"], default: 5_368_709_120, env: "KLOUDLITE_MAX_LAYER" },
    { name: "mergeConcurrency", description: "Merges the worker runs at once per pod.", unit: "jobs", range: { min: 1, max: 16 }, mark: "live", readers: ["kloudlite-worker"], default: 4, env: "KLOUDLITE_MERGE_CONCURRENCY" },
    { name: "maxTunnels", description: "Open workspace tunnels per gateway pod.", unit: "tunnels", range: { min: 100, max: 5000 }, mark: "live", readers: ["kloudlite-gateway"], default: 1000, env: "KLOUDLITE_MAX_TUNNELS" },
    { name: "cloneHost", description: "Hostname handed out in clone URLs.", unit: "", range: null, mark: "boot", readers: ["kloudlite-srv", "kloudlite-api"], default: "git.kloudlite.io", env: "KLOUDLITE_CLONE_HOST" },
  ],
  cluster: [
    { name: "syncSecs", description: "How often a running worktree is cut a sync point.", unit: "seconds", range: { min: 30, max: 3600 }, mark: "live", readers: ["kloudlite-agent"], default: 300, env: "WS_SYNC_SECS" },
    { name: "nodeDeadSecs", description: "Silence after which a node is unplaceable.", unit: "seconds", range: { min: 60, max: 1800 }, mark: "live", readers: ["kloudlite-agent"], default: 180, env: "WS_NODE_DEAD_SECS" },
    { name: "decommissionSecs", description: "Drain beat on a node that is leaving.", unit: "seconds", range: { min: 10, max: 300 }, mark: "live", readers: ["kloudlite-agent"], default: 30, env: "WS_DECOMMISSION_SECS" },
    { name: "retainSyncPoints", description: "Ready sync points kept per worktree.", unit: "cuts", range: { min: 1, max: 5 }, mark: "live", readers: ["kloudlite-agent"], default: 1, env: null },
    { name: "workspaceImage", description: "Image a workspace runs unless it names its own.", unit: "", range: null, mark: "boot", readers: ["kloudlite-agent"], default: "ghcr.io/kloudlite/kl-base:2026-08-21", env: "WS_IMAGE" },
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
  { _id: "karthik@kloudlite.io", addedAt: "2026-01-14T06:00:00Z", addedBy: "bootstrap" },
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
  usage: [3, 3, 4, 4, 4, 5, 5],
};

/** A region's own gauges differ from the fleet's, so a two-region page does not draw one line
 *  twice. The EU region runs colder — it was brought up for the load tests last week. */
const REGION_SCALE: Record<string, number> = { "westeurope-k3s": 0.55 };


// ── SLO probe ────────────────────────────────────────────────────────────────
// The catalogue below is `deploy/slo.md` verbatim — every id, feature, SLI sentence, target,
// suite and journey stage — because it is what the api derives both the SLO table and the
// journey from, and an invented id here would render a screen nobody can find in the catalogue.
// The manual rows (no id) are not probed and so are not here.
//
// One passing run, one failed run and one in flight, because those are the three shapes the
// screens have to render and a seed of only green runs proves nothing about the red path.

const mins = (m: number) => new Date(now - m * 60_000).toISOString();

/** id, feature, SLI, target, suite, journey stage. */
export const CATALOGUE: [string, string, string, string, string, string][] = [
  ["id.signin", "Identity", "Sign-in over HTTP succeeds", "99.9 %", "fast", "1 · Identity"],
  ["id.token.mint", "Identity", "Minting a user JWT succeeds", "99.9 %", "fast", "1 · Identity"],
  ["id.key.usable", "Identity", "A freshly minted platform SSH key is usable", "99.9 % ≤ 30000 ms", "fast", "1 · Identity"],
  ["id.cli.flow", "Identity", "The kl CLI's login-to-command flow completes", "99.9 % ≤ 15000 ms", "fast", "1 · Identity"],
  ["id.jwt.tiers", "Identity", "A JWT is honoured across every tier", "99.9 %", "fast", "1 · Identity"],
  ["id.signin.passkey", "Identity", "A passkey registers, lists back and its sign-in lookup is peer-only", "99.9 %", "fast", "1 · Identity"],
  ["git.push.ok", "Git hosting", "Push of one commit over HTTP succeeds", "99.9 %", "fast", "2 · Git"],
  ["git.push.p95", "Git hosting", "Push of one commit over HTTP completes", "95 % ≤ 3000 ms", "fast", "2 · Git"],
  ["git.clone.ok", "Git hosting", "Clone over HTTP succeeds", "99.9 %", "fast", "2 · Git"],
  ["git.clone.p95", "Git hosting", "Clone over HTTP completes", "95 % ≤ 2000 ms", "fast", "2 · Git"],
  ["ssh.clone.ok", "Git hosting", "Clone over SSH succeeds", "99.9 %", "fast", "2 · Git"],
  ["ssh.hostkey", "Git hosting", "The SSH host key served matches the pinned fingerprint", "99.9 %", "fast", "2 · Git"],
  ["ssh.unregistered.refused", "Git hosting", "SSH from an unregistered key is refused", "99.9 %", "fast", "2 · Git"],
  ["browse.p95", "Git hosting", "The Browse API renders a repo page", "95 % ≤ 500 ms", "fast", "2 · Git"],
  ["browse.commit.visible", "Git hosting", "A pushed commit becomes visible in Browse", "99.9 % ≤ 5000 ms", "fast", "2 · Git"],
  ["web.repo.page", "Git hosting", "The web app's repo page loads", "95 % ≤ 1500 ms", "fast", "2 · Git"],
  ["git.push.ssh", "Git hosting", "Push of one commit over SSH succeeds", "99.9 %", "fast", "2 · Git"],
  ["repo.lifecycle", "Git hosting", "A repo is created, listed, deleted and its slug freed", "99.9 % ≤ 10000 ms", "fast", "2 · Git"],
  ["web.org.page", "Git hosting", "The web app's org page loads", "95 % ≤ 1500 ms", "fast", "2 · Git"],
  ["web.repo.settings", "Git hosting", "The web app's repo settings page loads", "95 % ≤ 1500 ms", "fast", "2 · Git"],
  ["web.workspaces.page", "Workspaces", "The web app's workspaces and environments pages load", "95 % ≤ 1500 ms", "fast", "2 · Git"],
  ["pr.merge.p95", "Pull requests", "A pull request merge completes", "95 % ≤ 60000 ms", "fast", "3 · Pull request"],
  ["feed.latency", "Pull requests", "A PR event reaches the activity feed", "99.9 % ≤ 30000 ms", "fast", "3 · Pull request"],
  ["reg.token.p95", "Container registry", "Minting a registry bearer token completes", "95 % ≤ 300 ms", "fast", "4 · Registry"],
  ["reg.push.ok", "Container registry", "Pushing an image succeeds", "99.9 %", "fast", "4 · Registry"],
  ["reg.manifest.p95", "Container registry", "Fetching a manifest completes", "95 % ≤ 500 ms", "fast", "4 · Registry"],
  ["reg.tags.visible", "Container registry", "A pushed tag becomes visible in the tag list", "99.9 % ≤ 5000 ms", "fast", "4 · Registry"],
  ["reg.shared.layer", "Container registry", "A shared layer is not re-uploaded by a sibling image", "99.9 %", "fast", "4 · Registry"],
  ["reg.canary", "Container registry", "The registry canary image pulls successfully", "99.9 %", "fast", "4 · Registry"],
  ["reg.visibility", "Container registry", "Image visibility (public vs. private) is enforced", "99.9 %", "fast", "4 · Registry"],
  ["reg.image.delete", "Container registry", "Deleting a tag removes it from the tag list and deleting an image removes it from the catalogue", "99.9 % ≤ 10000 ms", "fast", "4 · Registry"],
  ["reg.catalogue", "Container registry", "The image catalogue lists a pushed image from any node", "99.9 % ≤ 5000 ms", "fast", "4 · Registry"],
  ["ws.create.p95", "Workspaces", "Creating a workspace completes", "95 % ≤ 90000 ms", "fast", "5 · Workspace"],
  ["ws.exec.ok", "Workspaces", "Exec into a running workspace pod returns the command's output, from a pod whose home is the shared export", "99.9 %", "fast", "5 · Workspace"],
  ["homes.rw.p95", "Workspaces", "A read/write round trip on the shared home completes", "95 % ≤ 200 ms", "fast", "5 · Workspace"],
  ["gw.tunnel.p95", "Workspaces", "Opening a gateway SSH tunnel completes", "95 % ≤ 3000 ms", "fast", "5 · Workspace"],
  ["gw.unregistered.refused", "Workspaces", "The gateway refuses an unregistered key", "99.9 %", "fast", "5 · Workspace"],
  ["ws.push.p95", "Workspaces", "Pushing a workspace snapshot completes", "95 % ≤ 60000 ms", "fast", "5 · Workspace"],
  ["ws.clone.p95", "Workspaces", "Cloning a workspace completes", "95 % ≤ 60000 ms", "fast", "5 · Workspace"],
  ["quota.refused", "Workspaces", "An over-quota create is refused with 409 naming the dimension, what is used and the limit", "99.9 %", "fast", "5 · Workspace"],
  ["env.quota.refused", "Workspaces", "An over-quota restore, clone and push are each refused with 409", "99.9 %", "fast", "5 · Workspace"],
  ["env.create.p95", "Environments", "Creating an environment completes", "95 % ≤ 120000 ms", "fast", "6 · Environment"],
  ["env.dns", "Environments", "A service in an environment resolves a sibling by bare name and connects to it", "99.9 %", "fast", "6 · Environment"],
  ["env.attach", "Environments", "Attaching a workspace to an environment takes effect", "99.9 % ≤ 10000 ms", "fast", "6 · Environment"],
  ["env.detach", "Environments", "Detaching a workspace from an environment takes effect", "99.9 % ≤ 10000 ms", "fast", "6 · Environment"],
  ["env.push.p95", "Environments", "Pushing an environment snapshot completes", "95 % ≤ 90000 ms", "fast", "6 · Environment"],
  ["env.exec.ok", "Environments", "Exec into a running service pod of the environment succeeds", "99.9 %", "fast", "6 · Environment"],
  ["env.clone.p95", "Environments", "Cloning a running environment completes with its services ready", "95 % ≤ 120000 ms", "fast", "6 · Environment"],
  ["ws.stop.p95", "Workspace lifecycle", "Stopping a workspace completes", "95 % ≤ 15000 ms", "fast", "7 · Lifecycle"],
  ["ws.replicated", "Workspace lifecycle", "A stopped workspace's final sync point reaches a replica, named by that replica", "99.9 % ≤ 300000 ms", "fast", "7 · Lifecycle"],
  ["ws.start.p95", "Workspace lifecycle", "Starting a workspace completes", "95 % ≤ 30000 ms", "fast", "7 · Lifecycle"],
  ["ws.restore", "Workspace lifecycle", "Restoring a workspace from a past snapshot succeeds", "99.9 %", "fast", "7 · Lifecycle"],
  ["env.stop.p95", "Environments", "Stopping an environment completes", "95 % ≤ 30000 ms", "fast", "7 · Lifecycle"],
  ["env.replicated", "Environments", "A stopped environment's final sync point reaches a replica", "99.9 % ≤ 300000 ms", "fast", "7 · Lifecycle"],
  ["env.start.p95", "Environments", "Starting an environment completes", "95 % ≤ 60000 ms", "fast", "7 · Lifecycle"],
  ["env.restore", "Environments", "Restoring an environment from a past snapshot succeeds", "99.9 %", "fast", "7 · Lifecycle"],
  ["vol.refusals", "Workspace lifecycle", "Deleting a sync point or a running worktree's base snapshot is refused", "99.9 %", "fast", "7 · Lifecycle"],
  ["vol.detached.restorable", "Workspace lifecycle", "A detached volume's snapshot can still be restored", "99.9 %", "fast", "7 · Lifecycle"],
  ["vol.orphan.collected", "Workspace lifecycle", "An orphaned volume directory is collected, and a Volume with no owner entry and no snapshot is deleted", "99.9 % ≤ 300000 ms", "fast", "7 · Lifecycle"],
  ["wt.delete", "Workspace lifecycle", "Deleting a workspace or environment drops the worktree and leaves the volume iff a snapshot remains", "99.9 % ≤ 60000 ms", "fast", "7 · Lifecycle"],
  ["snap.delete", "Workspace lifecycle", "Deleting a snapshot removes it from history, and the last one of a detached volume takes the volume with it", "99.9 %", "fast", "7 · Lifecycle"],
  ["req.queue", "Admin", "A Request CR is queued and answerable by an admin", "99.9 % ≤ 5000 ms", "fast", "8 · Admin"],
  ["audit.row", "Admin", "Every admin write produces an audit row, and the same write reaches `kloudlite.events` as `admin.<action>`", "99.9 %", "fast", "8 · Admin"],
  ["signals.fresh", "Admin", "The Signals table reflects a rule transition, and a rule with no covering samples reads `unknown` rather than `ok`", "99.9 % ≤ 120000 ms", "fast", "8 · Admin"],
  ["history.api", "Admin", "The history API answers a chart query", "99.9 %", "fast", "8 · Admin"],
  ["sec.private.repo", "Security", "A private repo is unreadable to a non-collaborator", "100 %", "fast", "9 · Security"],
  ["sec.cross.owner", "Security", "One owner's objects are invisible to another owner", "100 %", "fast", "9 · Security"],
  ["sec.admin.claim", "Security", "An admin route refuses a token without the superadmin claim", "100 %", "fast", "9 · Security"],
  ["sec.user.process", "Security", "The ordinary API process has no admin route mounted", "100 %", "fast", "9 · Security"],
  ["sec.agent.spec", "Security", "The admission policy refuses a spec write outside the allowed fields", "100 %", "fast", "9 · Security"],
  ["id.token.revoked", "Security", "A revoked token is refused", "99.9 %", "fast", "9 · Security"],
  ["repo.visibility", "Security", "A repo flipped private is hidden from a non-collaborator, and is hidden again after being flipped back", "100 %", "fast", "9 · Security"],
  ["repo.visibility.public", "Git hosting", "A repo flipped public becomes readable to another owner", "99.9 %", "fast", "9 · Security"],
  ["agent.spec.allowed", "Security", "The two spec writes the agent's ClusterRole grants are still admitted", "99.9 %", "fast", "9 · Security"],
  ["edge.dns", "Edge and pipeline", "The public hostname resolves", "99.99 %", "fast", "10 · Edge"],
  ["edge.cert", "Edge and pipeline", "The TLS certificate is valid for the public hostname", "99.9 %", "fast", "10 · Edge"],
  ["edge.origin", "Edge and pipeline", "Cloudflare reaches the origin", "99.9 %", "fast", "10 · Edge"],
  ["edge.ssh.lb", "Edge and pipeline", "The SSH load balancer accepts a connection", "99.9 %", "fast", "10 · Edge"],
  ["tel.log.latency", "Edge and pipeline", "A structured log line reaches HyperDX", "99.9 % ≤ 60000 ms", "fast", "10 · Edge"],
  ["tel.pod.coverage", "Edge and pipeline", "Every pod is scraped by the region's collector", "99.9 % ≤ 60000 ms", "fast", "10 · Edge"],
  ["tel.stream.lag", "Edge and pipeline", "The Redis events stream consumer lag stays low", "99.9 % ≤ 60000 ms", "fast", "10 · Edge"],
  ["tel.ch.disk", "Edge and pipeline", "ClickHouse disk usage is reported", "99.9 % ≤ 60000 ms", "fast", "10 · Edge"],
  ["git.push.large", "Git hosting", "Push of a large commit over HTTP succeeds", "99.9 %", "weekly", "12 · Weekly"],
  ["reg.push.large", "Container registry", "Pushing a large image layer succeeds", "99.9 %", "weekly", "12 · Weekly"],
  ["ws.cold.profile", "Workspaces", "A cold package profile builds successfully", "99.9 %", "weekly", "12 · Weekly"],
  ["ws.profile.reuse", "Workspaces", "A repeat package set is published from the profile index, not rebuilt", "99.9 %", "weekly", "12 · Weekly"],
  ["ws.cross.node", "Workspaces", "A workspace started on a peer node reads its replica correctly", "99.9 %", "weekly", "12 · Weekly"],
  ["homes.cross.node", "Workspaces", "The shared home is consistent across nodes", "99.9 %", "weekly", "12 · Weekly"],
  ["env.cross.node", "Environments", "An environment started on a peer node reads its replica correctly", "99.9 %", "weekly", "12 · Weekly"],
  ["cp.failover", "Control plane", "The leader lease fails over to another pod", "99.9 % ≤ 30000 ms", "weekly", "12 · Weekly"],
  ["settings.live", "Control plane", "A live settings change takes effect on the next beat", "99.9 % ≤ 60000 ms", "weekly", "12 · Weekly"],
  ["settings.revert", "Control plane", "Reverting to a stored settings version restores it", "99.9 % ≤ 60000 ms", "weekly", "12 · Weekly"],
  ["settings.roll", "Control plane", "A Boot-marked save is refused with 409 while one of its readers is mid-rollout, and nothing is written", "99.9 %", "weekly", "12 · Weekly"],
  ["reg.gc.sweep", "Container registry", "A blob a sibling image still references survives that image's deletion and a GC pass", "99.9 %", "weekly", "12 · Weekly"],
  ["bak.tarball.age", "Backups", "The latest backup tarball is recent", "99.9 %", "monthly", "13 · Monthly"],
  ["bak.daily.slots", "Backups", "Every daily backup slot is present", "99.9 %", "monthly", "13 · Monthly"],
  ["bak.versioning", "Backups", "Backup versioning is enabled and retains history", "99.9 %", "monthly", "13 · Monthly"],
  ["bak.cosmos", "Backups", "The Cosmos backup for HyperDX succeeds", "99.9 %", "monthly", "13 · Monthly"],
  ["drill.dead.node", "Resilience drills", "A dead-node drill heals every replica onto a live node", "99.9 %", "monthly", "13 · Monthly"],
  ["drill.drain", "Resilience drills", "A drain drill succeeds without interrupting a running worktree", "99.9 %", "monthly", "13 · Monthly"],
  ["drill.redis.down", "Resilience drills", "The system keeps operating correctly with Redis down", "99.9 %", "monthly", "13 · Monthly"],
  ["cluster.decommission", "Resilience drills", "A decommission is refused until the agent stamps `drained`, then cordons the node", "99.9 %", "monthly", "13 · Monthly"],
  ["ws.packages.add", "Workspaces", "Adding a package to a running workspace makes it runnable (`which`)", "95 % ≤ 180000 ms", "hourly", "14 · Experience"],
  ["ws.packages.remove", "Workspaces", "Removing it makes it disappear from the profile", "95 % ≤ 120000 ms", "hourly", "14 · Experience"],
  ["ws.seeded", "Workspaces", "A workspace created from a repo and branch has that clone checked out", "95 % ≤ 180000 ms", "hourly", "14 · Experience"],
  ["key.platform.regenerate", "Identity", "Regenerating the platform key keeps seeding working", "99.9 %", "hourly", "14 · Experience"],
  ["team.create", "Teams", "A team can be created by a person", "99.9 %", "hourly", "14 · Experience"],
  ["team.invite.accept", "Teams", "An invite is created, previewed and accepted once", "99.9 % ≤ 5000 ms", "hourly", "14 · Experience"],
  ["team.role.set", "Teams", "A member's role changes and is reflected in the profile", "99.9 %", "hourly", "14 · Experience"],
  ["team.repo.shared", "Teams", "A member clones a team repo; a non-member is refused", "99.9 %", "hourly", "14 · Experience"],
  ["team.workspace", "Teams", "A team workspace lands in the team namespace and starts", "95 % ≤ 90000 ms", "hourly", "14 · Experience"],
  ["team.member.remove", "Teams", "A removed member loses access to the team repo", "99.9 %", "hourly", "14 · Experience"],
  ["team.delete", "Teams", "Deleting the team removes its profile and refuses its slug", "99.9 %", "hourly", "14 · Experience"],
  ["repo.protection", "Git hosting", "A protected branch refuses a direct push and still merges via a PR", "99.9 %", "hourly", "14 · Experience"],
  ["repo.commit.patch", "Git hosting", "An edit made through the web commit endpoint lands in the log", "99.9 % ≤ 5000 ms", "hourly", "14 · Experience"],
  ["repo.compare", "Git hosting", "Comparing two branches lists the right commits", "99.9 % ≤ 1000 ms", "hourly", "14 · Experience"],
  ["pr.comment", "Pull requests", "A comment on a PR is readable back", "99.9 %", "hourly", "14 · Experience"],
  ["pr.close", "Pull requests", "A closed PR is refused a merge", "99.9 %", "hourly", "14 · Experience"],
  ["commit.verify", "Git hosting", "The signature endpoint answers for a pushed commit", "99.9 % ≤ 1000 ms", "hourly", "14 · Experience"],
  ["env.services.multi", "Environments", "An environment with two services has both ready and resolving each other", "95 % ≤ 180000 ms", "hourly", "14 · Experience"],
  ["env.clone", "Environments", "A stopped environment clones with all services ready", "95 % ≤ 180000 ms", "hourly", "14 · Experience"],
  ["env.restore.inplace", "Environments", "Restore in place brings a service's data back", "99.9 %", "hourly", "14 · Experience"],
  ["env.stop.start", "Environments", "Stop then start round trip", "95 % ≤ 120000 ms", "hourly", "14 · Experience"],
  ["vol.history", "Workspace lifecycle", "History lists pushes newest first with their messages; refs answer", "99.9 % ≤ 1000 ms", "hourly", "14 · Experience"],
  ["quota.view", "Admin", "`GET /v1/quota` reflects the objects the run holds", "99.9 %", "hourly", "14 · Experience"],
  ["request.approve", "Admin", "An approved quota request raises the quota and unblocks the refused create", "99.9 % ≤ 60000 ms", "hourly", "14 · Experience"],
  ["admin.stop.workspace", "Admin", "An admin stop is visible to the owner as `stopped`", "99.9 % ≤ 30000 ms", "hourly", "14 · Experience"],
  ["superadmin.grant", "Security", "Granting superadmin adds the account to the roster and revoking takes it off", "100 %", "hourly", "14 · Experience"],
  ["feed.experience", "Pull requests", "The feed shows the team and repo events of this run", "99.9 % ≤ 30000 ms", "hourly", "14 · Experience"],
  ["home.persists", "Workspaces", "A file written in one workspace is read from a fresh workspace's home, with the cache and state directories still local", "99.9 %", "hourly", "14 · Experience"],
  ["id.username", "Identity", "Claiming a username succeeds once and the second claim is refused", "99.9 %", "hourly", "14 · Experience"],
  ["id.cli.tokens", "Identity", "A CLI token is listed and, once revoked, is refused", "99.9 %", "hourly", "14 · Experience"],
  ["id.profile.upsert", "Identity", "A profile upsert is saved and read back", "99.9 % ≤ 5000 ms", "hourly", "14 · Experience"],
  ["id.cli.sshconfig", "Identity", "`kl ws sshconfig` writes a host block naming a running workspace", "99.9 % ≤ 15000 ms", "hourly", "14 · Experience"],
  ["key.ssh.lifecycle", "Identity", "A newly added SSH key clones, and after removal the same key is refused", "99.9 % ≤ 30000 ms", "hourly", "14 · Experience"],
  ["repo.description", "Git hosting", "A repo description is saved and read back", "99.9 % ≤ 5000 ms", "hourly", "14 · Experience"],
  ["pr.merge.strategies", "Pull requests", "Each merge strategy — merge, squash, rebase, fast-forward — lands the expected tree", "99.9 %", "hourly", "14 · Experience"],
  ["pr.mergeability", "Pull requests", "Mergeability is reported clean for a clean change and dirty for a conflicting one", "99.9 % ≤ 30000 ms", "hourly", "14 · Experience"],
  ["team.invite.revoke", "Teams", "A revoked invite token is refused", "100 %", "hourly", "14 · Experience"],
  ["team.environment", "Teams", "A team environment lands in the team namespace and its services resolve", "95 % ≤ 180000 ms", "hourly", "14 · Experience"],
  ["env.attach.pair", "Environments", "Deleting an attached workspace removes the environment-side policy", "99.9 % ≤ 30000 ms", "hourly", "14 · Experience"],
  ["vol.list", "Workspace lifecycle", "The volume list names every volume the run holds", "99.9 %", "hourly", "14 · Experience"],
  ["admin.stop.environment", "Admin", "An admin stop of an environment is visible to the owner as `stopped`", "99.9 % ≤ 30000 ms", "hourly", "14 · Experience"],
  ["admin.delete.workload", "Admin", "An admin delete takes a workspace and an environment away", "99.9 % ≤ 60000 ms", "hourly", "14 · Experience"],
  ["admin.screens", "Admin", "The owners, clusters and overview console screens answer", "99.9 % ≤ 10000 ms", "hourly", "14 · Experience"],
  ["admin.workloads.read", "Admin", "`GET /admin/workloads` lists every roll target", "99.9 % ≤ 5000 ms", "hourly", "14 · Experience"],
  ["audit.export", "Admin", "The audit CSV export answers with a header and this run's rows", "99.9 % ≤ 10000 ms", "hourly", "14 · Experience"],
  ["req.decide.kinds", "Admin", "An access request grants membership and a denied request is closed with its reason", "99.9 % ≤ 60000 ms", "hourly", "14 · Experience"],
  ["req.legacy.union", "Admin", "The retired quota-request queue is unioned into the admin queue and migrates", "99.9 % ≤ 10000 ms", "hourly", "14 · Experience"],
  ["region.status", "Admin", "The region list and this run's cluster status answer", "99.9 % ≤ 5000 ms", "hourly", "14 · Experience"],
];

/** The journey in order. Boot and teardown report no SLO and are stages all the same: they take
 *  time and they can fail, and a tree that hid them would show a run starting at its second act. */
const STAGES = [
  "0 · Boot",
  "1 · Identity",
  "2 · Git",
  "3 · Pull request",
  "4 · Registry",
  "5 · Workspace",
  "6 · Environment",
  "7 · Lifecycle",
  "8 · Admin",
  "9 · Security",
  "10 · Edge",
  "11 · Teardown",
];

const stage = (name: string, suites: string[]): SloJourneyStage => ({
  name,
  ids: CATALOGUE.filter((c) => c[5] === name && suites.includes(c[4])).map((c) => c[0]),
});

const FAST_JOURNEY = STAGES.map((s) => stage(s, ["fast"]));
const WEEKLY_JOURNEY = [...FAST_JOURNEY, stage("12 · Weekly", ["weekly"])];
const JOURNEY: SloJourney = {
  fast: FAST_JOURNEY,
  // Hourly is the fast journey plus Experience — it never walks the weekly or monthly stages.
  hourly: [...FAST_JOURNEY, stage("14 · Experience", ["hourly"])],
  weekly: WEEKLY_JOURNEY,
  monthly: [...WEEKLY_JOURNEY, stage("13 · Monthly", ["monthly"])],
};

/** The numbers the eleven interesting rows carry: attainment, budget left, both burn rates and the
 *  last measured ms. Everything else in the catalogue is a healthy row, because a table where every
 *  row is on fire is as useless a fixture as one where none is. */
const TUNED: Record<string, [number | null, number | null, [number | null, number | null], number]> = {
  "id.signin": [0.9994, 0.4, [0.3, 0.5], 640],
  "git.push.ok": [0.9982, 0.12, [4.1, 1.9], 2_310],
  "git.clone.p95": [0.9712, 0.44, [0.6, 0.7], 1_840],
  "reg.push.ok": [0.9971, -0.3, [7.4, 3.2], 5_120],
  "reg.manifest.p95": [0.9834, 0.66, [0.2, 0.3], 410],
  "ws.create.p95": [0.9683, 0.36, [0.6, 0.8], 61_400],
  "ws.push.p95": [null, null, [null, null], 0],
  "gw.tunnel.p95": [0.9761, 0.52, [0.4, 0.6], 2_120],
  "env.create.p95": [0.9604, 0.21, [1.2, 0.9], 96_800],
  "tel.log.latency": [0.9962, 0.62, [0.4, 0.4], 12_800],
  "cp.failover": [0.9941, 0.55, [null, 0.9], 14_200],
};
const STATES: Record<string, SloStatus["state"]> = {
  "git.push.ok": "burning",
  "reg.push.ok": "breaching",
  "ws.push.p95": "unknown",
};

/** The two window pairs the api reports: 1 h / 6 h for a suite that runs every 5 min, and
 *  4 w / 12 w for the slower ones, whose short column is a "—" because a 1 h window over a weekly
 *  sample could only ever be empty. */
const FAST: [number, number] = [3_600, 21_600];
const SLOW: [number, number] = [2_419_200, 7_257_600];

/** A stable pseudo-latency per id, so a step's ms and its SLO's last sample agree and neither
 *  moves between two screenshots of the same page. */
function msOf(id: string): number {
  const tuned = TUNED[id];
  if (tuned && tuned[3] > 0) return tuned[3];
  let h = 0;
  for (const ch of id) h = (h * 31 + ch.charCodeAt(0)) % 9_973;
  const ceiling = /≤ (\d+) ms/.exec(CATALOGUE.find((c) => c[0] === id)?.[3] ?? "");
  return ceiling ? Math.round(Number(ceiling[1]) * (0.4 + (h % 50) / 100)) : 120 + (h % 900);
}

const SLOS: SloStatus[] = CATALOGUE.map(([id, feature, sli, target, suite, ,]) => {
  const [attainment, budget, burn, ms] = TUNED[id] ?? [0.9991, 0.71, [0.3, 0.4], msOf(id)];
  const state = STATES[id] ?? "ok";
  const windows = suite === "fast" ? FAST : SLOW;
  return {
    id,
    feature,
    sli,
    target,
    suite,
    attainment_30d: attainment,
    total_30d: attainment == null ? 0 : suite === "fast" ? 8_640 : 12,
    budget_30d: budget == null ? 0 : Math.max(budget, 1),
    budget_left: budget,
    burn_short: suite === "fast" ? burn[0] : null,
    burn_long: burn[1],
    window_short_secs: windows[0],
    window_long_secs: windows[1],
    last: attainment == null ? null : { ts: mins(3), ok: state !== "breaching", ms },
    state,
  };
});

/** A run's steps, walked out of the catalogue itself: every id of every stage through `through`,
 *  in journey order, each stamped with the instant it would have finished at. After `failAt` every
 *  remaining step is SKIPPED rather than absent — that is what the probe reports, and it is the
 *  difference between "the registry broke the run" and "the run stopped for no stated reason". */
function walk(through: string, startedMins: number, suites: string[], failAt?: string): SloStep[] {
  const order = [...STAGES, "12 · Weekly", "13 · Monthly"];
  const out: SloStep[] = [];
  let t = now - startedMins * 60_000;
  let broke = "";
  for (const name of order.slice(0, order.indexOf(through) + 1)) {
    for (const [id, , , , suite, at] of CATALOGUE) {
      if (at !== name || !suites.includes(suite)) continue;
      const skipped = broke !== "";
      const ok = !skipped && id !== failAt;
      const ms = skipped ? 0 : msOf(id);
      out.push({
        slo_id: id,
        stage: name,
        ts: new Date((t += ms + 300)).toISOString(),
        ok,
        ms,
        skipped,
        detail: skipped
          ? `skipped: the ${broke} stage failed`
          : ok
            ? ""
            : "manifest PUT answered 500 after 5.0 s (image img/acme/api, node kl-1); the layer upload before it succeeded, so the blob is in the store and the tag is not",
      });
      if (id === failAt) broke = name;
    }
  }
  return out;
}

const PASSED_STEPS = walk("11 · Teardown", 190, ["fast"]);
const FAILED_STEPS = walk("11 · Teardown", 74, ["fast"], "reg.push.ok");
// Mid stage 5: the panel's whole point is a run caught in the act, half a stage in.
const RUNNING_STEPS = walk("5 · Workspace", 3, ["fast"]).slice(0, 28);
const WEEKLY_STEPS = walk("12 · Weekly", 190, ["fast", "weekly"]);

const total = (steps: SloStep[]) => steps.reduce((a, s) => a + s.ms + 300, 0);

/** The probe's own shape: the suite it ran and the unix second it started at. */
const runId = (suite: string, startedMins: number) => `${suite}-${Math.floor((now - startedMins * 60_000) / 1000)}`;

const run = (suite: string, state: SloRun["state"], at: string, startedMins: number, steps: SloStep[]): SloRun => ({
  run_id: runId(suite, startedMins),
  suite,
  region: "centralindia-k3s",
  started: mins(startedMins),
  finished: state === "running" ? null : new Date(now - startedMins * 60_000 + total(steps)).toISOString(),
  state,
  stage: at,
  steps_total: steps.length,
  steps_failed: steps.filter((s) => !s.ok && !s.skipped).length,
  failed_step: steps.find((s) => !s.ok && !s.skipped)?.slo_id ?? "",
  failed_detail: steps.find((s) => !s.ok && !s.skipped)?.detail ?? "",
  duration_ms: total(steps),
});

const RUNNING_RUN = run("fast", "running", "5 · Workspace", 3, RUNNING_STEPS);
const FAILED_RUN = run("fast", "failed", "4 · Registry", 74, FAILED_STEPS);
const WEEKLY_RUN = run("weekly", "passed", "12 · Weekly", 190, WEEKLY_STEPS);
const FAST_RUNS: SloRun[] = [12, 27, 42, 57].map((m) => run("fast", "passed", "11 · Teardown", m, PASSED_STEPS));

const SLO_RUNS: SloRun[] = [RUNNING_RUN, ...FAST_RUNS, FAILED_RUN, WEEKLY_RUN];

const SLO_OVERVIEW: SloOverview = {
  slos: SLOS,
  running: RUNNING_RUN,
  runs: SLO_RUNS,
  journey: JOURNEY,
  generated: new Date(now).toISOString(),
};

const SLO_DETAILS: Record<string, SloRunDetail> = Object.fromEntries(
  [
    [RUNNING_RUN, RUNNING_STEPS] as const,
    [FAILED_RUN, FAILED_STEPS] as const,
    [WEEKLY_RUN, WEEKLY_STEPS] as const,
    ...FAST_RUNS.map((r) => [r, PASSED_STEPS] as const),
  ].map(([r, steps]) => [
    r.run_id,
    { ...r, steps, journey: JOURNEY[r.suite as keyof SloJourney] ?? JOURNEY.fast },
  ]),
);


const EXACT: Record<string, unknown> = {
  "/admin/overview": OVERVIEW,
  "/admin/owners": OWNERS,
  "/admin/clusters": CLUSTERS,
  "/admin/workloads": WORKLOADS,
  "/admin/monitoring/signals": SIGNALS,
  "/admin/slo": SLO_OVERVIEW,
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
  if (bare === "/admin/slo/runs") {
    const q = new URLSearchParams(query);
    const suite = q.get("suite");
    const limit = Number(q.get("limit") ?? SLO_RUNS.length);
    return SLO_RUNS.filter((r) => !suite || r.suite === suite).slice(0, limit);
  }
  if (bare.startsWith("/admin/slo/runs/")) {
    // Run ids carry the unix second the run started, so they differ on every process start and
    // nothing can link to one. An unseeded id answers with the failed run rather than 404ing, so
    // the screenshot script has a stable URL for the run page — the one screen whose whole point
    // is a failure somebody has to read.
    const id = decodeURIComponent(bare.slice("/admin/slo/runs/".length));
    return SLO_DETAILS[id] ?? SLO_DETAILS[FAILED_RUN.run_id];
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

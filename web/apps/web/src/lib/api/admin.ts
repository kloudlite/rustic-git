import type { QuotaDim } from "@/lib/quota";
import { auditQueryString, type AuditEntry, type AuditFilter, type AuditPage } from "@/lib/audit";
import { FLAT, type HistoryEvent, type HistorySeries, type SeriesName } from "@/lib/history";
import { ADMIN_BASE, adminCall, call } from "./client";
import type { ApiEnvironment, ApiVolumeSummary, ApiWorkspace, QuotaRequestDoc } from "./workspaces";

/**
 * The /superadmin console's reads and writes, against the admin host only.
 */
/** `GET /admin/owners` — every owner's usage against their limit, tightest-first (the api's own
 *  sort, never re-sorted here except by the list's own controls). */
export type OwnerRow = {
  owner: string;
  isTeam: boolean;
  limit: Record<QuotaDim, number>;
  used: Record<QuotaDim, number>;
  /** `"own"` when the owner has an explicit `Quota`, `"default"` when riding the fallback table. */
  source: "own" | "default";
  /** A `QuotaRequest` still pending for this owner. */
  pending: boolean;
};

export function adminOwners(token: string) {
  return adminCall<OwnerRow[]>("/admin/owners", { method: "GET", token });
}

/** `GET /admin/owners/{slug}` — everything the detail page shows without a second click.
 *  `requests` and `audit` are already truncated server-side (last 5 / last 10); the page links to
 *  the Requests and Audit areas, filtered to this owner, for the rest. */
export type OwnerDetail = OwnerRow & {
  workspaces: ApiWorkspace[];
  environments: ApiEnvironment[];
  volumes: ApiVolumeSummary[];
  requests: QuotaRequestDoc[];
  audit: AuditEntry[];
};

export function adminOwnerDetail(slug: string, token: string) {
  return adminCall<OwnerDetail>(`/admin/owners/${encodeURIComponent(slug)}`, { method: "GET", token });
}

// `/admin/workspaces/{id}` and `/admin/environments/{id}` reuse the SAME handlers `/v1` calls for
// the caller's own objects, just with the owner taken from the object rather than the token — see
// `crates/workspaces/src/api/admin.rs`'s "cross-owner list / stop / delete" section, wrapped there
// to take the note every admin write carries — acting on somebody else's working copy is the
// loudest thing this console does, and the api 422s an empty one.
export function adminStopWorkspace(id: string, token: string, note: string) {
  return adminCall<ApiWorkspace>(`/admin/workspaces/${encodeURIComponent(id)}/stop`, { method: "POST", token, body: JSON.stringify({ note }) });
}

export function adminDeleteWorkspace(id: string, token: string, note: string) {
  return adminCall<ApiWorkspace>(`/admin/workspaces/${encodeURIComponent(id)}`, { method: "DELETE", token, body: JSON.stringify({ note }) });
}

export function adminStopEnvironment(id: string, token: string, note: string) {
  return adminCall<ApiEnvironment>(`/admin/environments/${encodeURIComponent(id)}/stop`, { method: "POST", token, body: JSON.stringify({ note }) });
}

export function adminDeleteEnvironment(id: string, token: string, note: string) {
  return adminCall<ApiEnvironment>(`/admin/environments/${encodeURIComponent(id)}`, { method: "DELETE", token, body: JSON.stringify({ note }) });
}

export type AdminNode = { name: string; ready: boolean; decommission: boolean; decommissionStatus: string | null };

export function adminAudit(token: string, filter: AuditFilter) {
  return adminCall<AuditPage>(`/admin/audit${auditQueryString(filter)}`, { method: "GET", token });
}

/** Raw `Response`, not `ApiResult` — the CSV export route streams this straight through to the
 *  browser rather than parsing it, so it needs the actual body and status, not `adminCall`'s
 *  JSON-shaped envelope. The one caller that talks to `ADMIN_BASE` directly. */
export function adminAuditCsv(token: string, filter: AuditFilter): Promise<Response> {
  return fetch(`${ADMIN_BASE}/admin/audit.csv${auditQueryString(filter)}`, {
    headers: { authorization: `Bearer ${token}` },
    cache: "no-store",
  });
}

/** One row of `GET /admin/settings/schema` — `crates/workspaces/src/api/admin/schema.rs::Row`.
 *  `range` is `null` for a bool/string field, where a min/max means nothing. */
export type SettingsSchemaRow = {
  name: string;
  description: string;
  unit: string;
  range: { min: number; max: number } | null;
  mark: "live" | "boot";
  readers: string[];
  default: unknown;
  env: string | null;
};

export type SettingsSchema = { central: SettingsSchemaRow[]; cluster: SettingsSchemaRow[] };

export function adminSettingsSchema(token: string) {
  return adminCall<SettingsSchema>("/admin/settings/schema", { method: "GET", token });
}

/** `crates/core/settings::StoredCentralSettings` — every field `Option`, `null`/absent meaning
 *  "never set" (falls back to env, then the built-in default). Read-only here: keyed dynamically
 *  against `SettingsSchemaRow.name` rather than re-typing every field, same as `adminClusterSettings`. */
export function adminCentralSettings(token: string) {
  return adminCall<Record<string, unknown>>("/admin/settings/central", { method: "GET", token });
}

/** `GET /admin/settings/clusters/{region}` returns the whole `ClusterSettings` CR; only `.spec`
 *  (same `stored ?? env ?? default` fields as the central document) matters for display. */
export function adminClusterSettings(region: string, token: string) {
  return adminCall<{ spec: Record<string, unknown> }>(`/admin/settings/clusters/${encodeURIComponent(region)}`, {
    method: "GET",
    token,
  });
}

export function createRegion(body: { id: string; name: string; note: string }, token: string) {
  return adminCall<{ id: string; name: string; status: string }>("/admin/regions", {
    method: "POST",
    token,
    body: JSON.stringify(body),
  });
}

// ── clusters (admin host — crates/workspaces/src/api/admin/clusters.rs) ──

/** `GET /admin/clusters` — one row per region, everything the Clusters list card needs without a
 *  second click. `settingsStatus` is an open string (`"present"`, or `"stale (lag N)"` when the
 *  agents have not caught up) — render via `lib/clusters.ts::settingsStatusTone` rather than
 *  matching it here. */
export type AdminClusterRow = {
  region: string;
  status: string;
  agentsReady: number;
  agentsDesired: number;
  nodesReady: number;
  nodesTotal: number;
  draining: number;
  workingCopies: number;
  settingsStatus: string;
};

export function adminClusters(token: string) {
  return adminCall<AdminClusterRow[]>("/admin/clusters", { method: "GET", token });
}

/** `GET /admin/clusters/{region}` node row — `NodeDoc`'s four fields flattened, plus what a drain
 *  is waiting for: live working copies and replicas held on this node. */
export type AdminClusterNode = {
  name: string;
  ready: boolean;
  decommission: boolean;
  decommissionStatus: string | null;
  workingCopies: number;
  replicasHeld: number;
};

export type AdminClusterDetail = {
  region: string;
  status: string;
  nodes: AdminClusterNode[];
  workloads: WorkloadDoc[];
  settings: Record<string, unknown>;
};

export function adminClusterDetail(region: string, token: string) {
  return adminCall<AdminClusterDetail>(`/admin/clusters/${encodeURIComponent(region)}`, { method: "GET", token });
}

/** Activate/deactivate — server-side apply of the same shape `createRegion` writes. `note` is
 *  required only for `"inactive"` (a required reason on the loud half, per the Global Constraint);
 *  the api 422s a missing one itself. */
export function adminSetRegionStatus(region: string, status: "active" | "inactive", note: string, token: string) {
  return adminCall<{ id: string; name: string; status: string }>(
    `/admin/clusters/${encodeURIComponent(region)}/status`,
    { method: "PUT", token, body: JSON.stringify({ status, note }) },
  );
}

export function nodeVerb(verb: "drain" | "undrain" | "decommission", region: string, node: string, reason: string, token: string) {
  return adminCall<AdminNode>(
    `/admin/clusters/${encodeURIComponent(region)}/nodes/${encodeURIComponent(node)}/${verb}`,
    { method: "POST", token, body: JSON.stringify({ reason }) },
  );
}

/** Sets the label the agent already watches — the drain itself runs on the node's own beat
 *  (CLAUDE.md, "Workspaces and environments"). `reason` is required; the api 422s an empty one. */
export function adminDrainNode(region: string, node: string, reason: string, token: string) {
  return nodeVerb("drain", region, node, reason, token);
}

/** A real abort — clears both the label and any `decommission-status` stamp, so a drain that
 *  never finished cannot leave a stale gate open for decommission. */
export function adminUndrainNode(region: string, node: string, reason: string, token: string) {
  return nodeVerb("undrain", region, node, reason, token);
}

/** Cordons the node (`spec.unschedulable`) and nothing else — the console never deletes the VM.
 *  409 "not drained yet" when `decommissionStatus` hasn't reached `"drained …"`. */
export function adminDecommissionNode(region: string, node: string, reason: string, token: string) {
  return nodeVerb("decommission", region, node, reason, token);
}

// ── workloads (admin host — crates/workspaces/src/api/admin/settings.rs) ─

/** `crates/workspaces/src/api/workloads.rs::WorkloadDoc`. `scope` serializes as a plain string
 *  now (`Scope`'s hand-written `Serialize`) — `"central"` or the bare region id — the fix for the
 *  internally-tagged-enum panic Task 7 hit is in (`fd9e851a`). */
export type WorkloadDoc = {
  scope: string;
  name: string;
  kind: "statefulset" | "deployment" | "daemonset";
  image: string | null;
  ready: number;
  desired: number;
  rolloutState: "RollingOut" | "Stable";
  lastRoll: { by: string; at: string; reason: string } | null;
};

export function listWorkloads(token: string) {
  return adminCall<WorkloadDoc[]>("/admin/workloads", { method: "GET", token });
}

/** `POST /admin/workloads/{scope}/{name}/roll` — the one write the Workloads tab offers, a
 *  manual restart with a required reason (`crates/workspaces/src/api/admin.rs::roll_workload_route`
 *  400s an empty one). `scope` is `"central"` or a region id, same encoding as `WorkloadDoc.scope`. */
export function rollWorkload(scope: string, name: string, reason: string, token: string) {
  return adminCall<WorkloadDoc>(`/admin/workloads/${encodeURIComponent(scope)}/${encodeURIComponent(name)}/roll`, {
    method: "POST",
    token,
    body: JSON.stringify({ reason }),
  });
}

/** `crates/workspaces/src/api/admin/monitoring.rs::SignalRow` — one catalogue rule
 *  (`deploy/alerts.md`), evaluated by scraping every pod's `/metrics` on the request path rather
 *  than through Prometheus. `detail` is the observed numbers behind `state`, or why a rule that
 *  needs a window this process cannot see stayed `unknown` — never guessed as `ok`. */
export type SignalRow = {
  alert: string;
  state: "firing" | "ok" | "unknown";
  why: string;
  detail: string | null;
  /** Which region this evaluation scraped, or `null` for a fleet-wide (central) rule — lets the
   *  toolbar group a per-region catalogue without a second fetch. */
  region: string | null;
};

/** `Restarts` in the same handler — container restart count since each pod started (Kubernetes
 *  exposes no 1 h window), summed per KNOWN central workload. */
export type SignalRestarts = { workload: string; restarts: number };

export type SignalsResponse = {
  signals: SignalRow[];
  restarts: SignalRestarts[];
  // Field names are the wire ones verbatim — `SignalsResponse` has no `rename_all`, unlike
  // most admin responses.
  scrape_failures: [string, string][];
  pods_listed: number;
  /** Absent (not null — `skip_serializing_if`) unless `KLOUDLITE_HYPERDX_URL` is configured, so
   *  a monitoring page never renders a dead link. */
  hyperdx_url?: string;
};

export function adminMonitoringSignals(token: string) {
  return adminCall<SignalsResponse>("/admin/monitoring/signals", { method: "GET", token });
}

/** `crates/workspaces/src/api/admin/overview.rs::AttentionItem` — no `rename_all`, so its three
 *  fields are already the wire shape verbatim. */
export type AttentionItem = { kind: string; detail: string; href: string };

/** `RegionFleet`/`FleetNumbers`, both `rename_all = "camelCase"`. */
export type RegionFleet = { owners: number; workspaces: number; environments: number; snapshots: number; diskGb: number };
export type FleetNumbers = {
  owners: number;
  workspaces: number;
  environments: number;
  snapshots: number;
  diskGbTotal: number;
  perRegion: Record<string, RegionFleet>;
};

/** `Overview`, `rename_all = "camelCase"`. `errors` is `skip_serializing_if = "Vec::is_empty"`,
 *  so it is absent rather than `[]` when every sub-source read cleanly. */
export type Overview = {
  pendingRequests: QuotaRequestDoc[];
  attention: AttentionItem[];
  recentAudit: AuditEntry[];
  fleet: FleetNumbers;
  errors?: string[];
};

export function adminOverview(token: string) {
  return adminCall<Overview>("/admin/overview", { method: "GET", token });
}

// ── history (admin host — crates/workspaces/src/api/admin/history.rs, spec §A5) ─

/** `GET /admin/history/{series}`. Deliberately NOT an `ApiResult`: history is optional
 *  infrastructure (a `503 history unavailable` when the admin process has no ClickHouse URL), and
 *  a page that reads five series must not have five failure branches. Every non-ok answer is the
 *  same flat placeholder, which every tile already knows how to render. */
export async function adminSeries(
  name: SeriesName,
  opts: { range?: string; step?: string; region?: string; owner?: string; dimension?: string },
  token: string,
): Promise<HistorySeries> {
  const qs = new URLSearchParams();
  qs.set("range", opts.range ?? "7d");
  qs.set("step", opts.step ?? "1d");
  if (opts.region) qs.set("region", opts.region);
  if (opts.owner) qs.set("owner", opts.owner);
  if (opts.dimension) qs.set("dimension", opts.dimension);
  const r = await adminCall<Omit<HistorySeries, "available">>(
    `/admin/history/${encodeURIComponent(name)}?${qs}`,
    { method: "GET", token },
  );
  return r.ok ? { ...r.value, available: true } : FLAT;
}

/** `GET /admin/history/events` — the timeline and the activity feed. This one keeps its
 *  `ApiResult`: a section whose whole content is events says so in its own empty state. */
export function adminHistoryEvents(
  q: { kind?: string; owner?: string; region?: string; from?: string; to?: string; cursor?: string; limit?: number },
  token: string,
) {
  const qs = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== "") qs.set(k, String(v));
  return adminCall<{ events: HistoryEvent[]; cursor: string | null }>(
    `/admin/history/events${qs.toString() ? `?${qs}` : ""}`,
    { method: "GET", token },
  );
}

/** Display-only slice of the central document — `crates/api/src/lib.rs::settings_central`, the
 *  UNAUTHENTICATED route on the ordinary api host (not `/admin`), so `lib/clone.ts` can call it
 *  without a signed-in caller's token. Blank fields mean "never set", the same fallback-to-env
 *  contract that route's own doc comment states. */
export type PublicCentralSettings = { cloneHost: string; sshHost: string; sshPort: number; registryHost: string };

export function getPublicCentralSettings() {
  return call<PublicCentralSettings>("/v1/settings/central", { method: "GET" });
}

// ── superadmins (server tier, not the admin host — crates/api/src/teams.rs) ─

export type SuperAdmin = { _id: string; addedAt: string; addedBy: string };

export function listSuperadmins(token: string) {
  return call<SuperAdmin[]>("/api/admin/superadmins", { method: "GET", token });
}

// A required note (Global Constraint: reason on every write except approve) — the api 422s an
// empty one, and that message surfaces to the form rather than being swallowed.
export function addSuperadmin(user: string, token: string, note: string) {
  return call<undefined>(`/api/admin/superadmins/${encodeURIComponent(user)}`, {
    method: "POST",
    token,
    body: JSON.stringify({ note }),
  });
}

export function removeSuperadmin(user: string, token: string, note: string) {
  return call<undefined>(`/api/admin/superadmins/${encodeURIComponent(user)}`, {
    method: "DELETE",
    token,
    body: JSON.stringify({ note }),
  });
}


// ── SLO probe (`crates/workspaces/src/history/slo.rs`) ────────────────────────
// None of these structs carry `rename_all`, so every field below is the wire name verbatim.

export type SloRunState = "running" | "passed" | "failed" | "yielded";

/** One row of `slo_runs`. `finished` is `null` while the run is in flight, and `duration_ms` is
 *  then the elapsed time so far — the probe recomputes it on every report. */
export type SloRun = {
  run_id: string;
  suite: string;
  region: string;
  started: string;
  finished: string | null;
  state: SloRunState;
  stage: string;
  steps_total: number;
  steps_failed: number;
  failed_step: string;
  failed_detail: string;
  duration_ms: number;
  /** The row's own heartbeat, written on every report. A `running` row whose `updated` has gone
   *  stale belongs to a pod that is gone — which is how the probe tells one from a slow run. */
  updated?: string | null;
};

/** A `skipped` step is stored but counts neither way — the probe could not attempt it. */
export type SloStep = {
  slo_id: string;
  ts: string;
  ok: boolean;
  ms: number;
  skipped: boolean;
  detail: string;
  /** The journey stage the step ran in ("5 · Workspace"), which is what the tracker groups by. */
  stage: string;
};

/** `attainment_30d`, `budget_left` and both burn rates are `null` when the window holds no sample
 *  at all — a fresh cluster, never 0 %. A weekly SLO has no short window either, so its
 *  `burn_short` stays `null` while `window_short_secs` is still reported. */
export type SloStatus = {
  id: string;
  feature: string;
  sli: string;
  target: string;
  suite: string;
  attainment_30d: number | null;
  total_30d: number;
  /** Bad samples the window can afford; 0 for a 100 % target. A count, like `budget_left`. */
  budget_30d: number;
  budget_left: number | null;
  burn_short: number | null;
  burn_long: number | null;
  window_short_secs: number;
  window_long_secs: number;
  last: { ts: string; ok: boolean; ms: number } | null;
  state: "ok" | "burning" | "breaching" | "unknown";
};

/** One stage of the journey the probe walks, in journey order, derived from the catalogue's
 *  "Journey step" column. `ids` is what the stage WILL report, which is the only way the console
 *  can draw a step that has not happened yet — a run's own steps say nothing about what is left.
 *  A stage with no ids (boot, teardown) is real: it takes time and can fail. */
export type SloJourneyStage = { name: string; ids: string[] };

/** Per suite, because every other suite walks the fast journey plus its own stage — hourly adds
 *  Experience alone, weekly adds Weekly, monthly adds both. */
export type SloJourney = {
  fast: SloJourneyStage[];
  hourly: SloJourneyStage[];
  weekly: SloJourneyStage[];
  monthly: SloJourneyStage[];
};

export type SloOverview = {
  slos: SloStatus[];
  running: SloRun | null;
  runs: SloRun[];
  journey: SloJourney;
  generated: string;
};

/** `journey` here is the one for THIS run's suite, already picked by the api. */
export type SloRunDetail = SloRun & { steps: SloStep[]; journey: SloJourneyStage[] };

/** The whole area in one call — the console polls it every 10 s, and three requests would be
 *  three chances for the page to render halves of different moments. */
export function adminSlo(token: string) {
  return adminCall<SloOverview>("/admin/slo", { method: "GET", token });
}

export function adminSloRuns(token: string, opts: { suite?: string; limit?: number } = {}) {
  const q = new URLSearchParams();
  if (opts.suite) q.set("suite", opts.suite);
  if (opts.limit) q.set("limit", String(opts.limit));
  const qs = q.toString();
  return adminCall<SloRun[]>(`/admin/slo/runs${qs ? `?${qs}` : ""}`, { method: "GET", token });
}

export function adminSloRun(token: string, id: string) {
  return adminCall<SloRunDetail>(`/admin/slo/runs/${encodeURIComponent(id)}`, { method: "GET", token });
}

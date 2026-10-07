/**
 * MOCK DATA — placeholder shape only, so the sidebar has something to render
 * while we settle the UI. Nothing here reflects real kloudlite semantics yet;
 * replace wholesale once attach/clone is designed.
 */
export type WorkspaceStatus = "running" | "attached" | "stopped" | "cloning";

export type Workspace = {
  id: string;
  name: string;
  /** User that owns (and can attach to) this workspace. */
  owner: string;
  /**
   * Main session this workspace hangs off. The hierarchy is
   * environment › main session › workspaces — switching main sessions swaps
   * which workspaces are in view. Undefined means the default "main" session.
   */
  session?: string;
  status: WorkspaceStatus;
  /**
   * Parent workspace id — set only on ephemeral workspaces. The hierarchy is
   * exactly three levels: main session › workspaces › ephemeral workspaces
   * (spun off a workspace to test something, like a git worktree). The flat
   * list stays in tree order, so a workspace's ephemerals follow it directly.
   */
  parent?: string;
  /** Ephemeral only: what the agent in it is doing. */
  task?: string;
  /** Clone progress, e.g. "42%" — only meaningful while status is "cloning". */
  progress?: string;
  /** Ports the workspace exposes. */
  ports: number[];
  /** Long-running processes inside the workspace (dev servers, watchers). */
  processes?: Process[];
  /** Uncommitted files in the workspace's checkout. */
  changes?: number;
  repo: string;
  branch: string;
};

export type ProcessStatus = "running" | "starting" | "exited" | "crashed";

/** A process the workspace runs — what the backend is actually doing. */
export type Process = {
  name: string;
  command: string;
  status: ProcessStatus;
  /** Port it listens on, when it serves one. */
  port?: number;
  /** Exit code, for a process that stopped. */
  code?: number;
  /** Recent output, oldest first. */
  logs: string[];
};

export type Service = {
  name: string;
  port: number;
  /** Protocol shown with the port; defaults to tcp. */
  proto?: "http" | "tcp";
  /** Workspace currently intercepting this service's traffic, if any. */
  interceptedBy?: string;
};

/**
 * An environment is somewhere a working session can plug in: its services, and
 * nothing else. **Workspaces do not belong to an environment** — they belong to
 * the working session, which carries them whole when it connects elsewhere, so
 * another environment's workspaces can never appear in this session's list.
 */
export type Environment = {
  id: string;
  name: string;
  /** User (or team) that owns this environment. */
  owner: string;
  services: Service[];
  /**
   * The restore point the environment is sitting on. The agent takes and
   * switches these; the UI only ever shows which one is in effect, never a
   * list of them.
   */
  snapshot?: string;
};

/** The signed-in user (mock until kloudlite auth is wired). */
export const CURRENT_USER = "karthik";

/**
 * The workspace an ephemeral one should hang off: a workspace parents itself,
 * an ephemeral hands its own parent over, so nothing nests deeper than one.
 */
export function parentFor(workspaces: Workspace[], w: Workspace): string {
  return w.parent && workspaces.some((p) => p.id === w.parent) ? w.parent : w.id;
}

/** The workspace, preceded by its parent when it is an ephemeral one. */
export function wsPath(workspaces: Workspace[], w: Workspace): string[] {
  const parent = w.parent ? workspaces.find((p) => p.id === w.parent) : undefined;
  return parent ? [parent.name, w.name] : [w.name];
}

/** Display label: own environments by name, others as owner/name. */
export function envLabel(e: Environment): string {
  return e.owner === CURRENT_USER ? e.name : `${e.owner}/${e.name}`;
}

/** The working session's workspaces — they move with it, never with an environment. */
export const MOCK_WORKSPACES: Workspace[] = [
  { id: "w1", name: "api-gateway", owner: "karthik", status: "attached", ports: [8080, 9090], repo: "kloudlite/api-gateway", branch: "feat/rate-limits", changes: 12, processes: [
    { name: "server", command: "go run ./cmd/gateway", status: "running", port: 8080, logs: [
      "listening on :8080",
      "route  GET  /healthz",
      "route  POST /v1/tokens",
      "rate limiter: 100 req/s per key",
      "GET /healthz 200 1.2ms",
      "POST /v1/tokens 201 18.4ms",
    ] },
    { name: "metrics", command: "go run ./cmd/metrics", status: "running", port: 9090, logs: [
      "serving /metrics on :9090",
      "scrape from 10.0.2.14 200 0.8ms",
    ] },
    { name: "tests", command: "go test ./... -watch", status: "crashed", code: 1, logs: [
      "--- FAIL: TestRateLimit (0.03s)",
      "    limiter_test.go:42: want 100 got 128",
      "FAIL  github.com/kloudlite/api-gateway/limiter",
      "exit status 1",
    ] },
  ] },
  { id: "w1a", name: "rate-limits-probe", owner: "karthik", parent: "w1", task: "Filter the OwnerBinding watch to owned namespaces", status: "running", ports: [8090], repo: "kloudlite/api-gateway", branch: "feat/rate-limits-probe", changes: 3, processes: [
    { name: "probe", command: "go run ./cmd/probe", status: "running", port: 8090, logs: [
      "probing api-gateway:8080 every 5s",
      "p99 latency 42ms over 120 samples",
    ] },
  ] },
  { id: "w1b", name: "load-test", owner: "karthik", parent: "w1", task: "Review bc5a5062 against the rate-limit spec", status: "stopped", ports: [], repo: "kloudlite/api-gateway", branch: "feat/rate-limits-probe", changes: 0 },
  { id: "w2", name: "billing-svc", owner: "karthik", status: "running", ports: [8081], repo: "kloudlite/billing-svc", branch: "main", changes: 5, processes: [
    { name: "server", command: "bun run dev", status: "running", port: 8081, logs: [
      "bun v1.3.14 ready in 41ms",
      "listening on http://localhost:8081",
    ] },
    { name: "migrate", command: "bun run migrate", status: "exited", code: 0, logs: [
      "applied 3 migrations",
      "schema at revision 2026_08_31_a",
    ] },
  ] },
  { id: "w3", name: "console-web", owner: "karthik", status: "cloning", progress: "42%", ports: [], repo: "kloudlite/console-web", branch: "main", changes: 0, processes: [
    { name: "vite", command: "bun run dev", status: "starting", port: 3000, logs: [
      "installing dependencies…",
    ] },
  ] },
  { id: "w4", name: "infra-iac", owner: "karthik", status: "stopped", ports: [], repo: "kloudlite/infra-iac", branch: "main", changes: 1 },
];

export const MOCK_ENVIRONMENTS: Environment[] = [
  {
    id: "e1",
    name: "production",
    owner: "karthik",
    snapshot: "pre-rate-limits",
    services: [
      { name: "api", port: 8080, proto: "http", interceptedBy: "api-gateway" },
      { name: "postgres", port: 5432 },
      { name: "redis", port: 6379 },
      { name: "console", port: 3000, proto: "http" },
      { name: "clickhouse", port: 8123, proto: "http" },
    ],
  },
  {
    id: "e2",
    name: "staging",
    owner: "karthik",
    services: [
      { name: "postgres", port: 5432 },
      { name: "api", port: 8080, interceptedBy: "api-gateway" },
    ],
  },
  {
    id: "e4",
    name: "qa",
    owner: "karthik",
    services: [
      { name: "postgres", port: 5432 },
      { name: "api", port: 8080 },
    ],
  },
  {
    id: "e3",
    name: "dev-karthik",
    owner: "karthik",
    services: [{ name: "postgres", port: 5432 }],
  },
  // other users' environments — workspaces can connect into them
  {
    id: "e5",
    name: "payments",
    owner: "sara",
    services: [
      { name: "postgres", port: 5432 },
      { name: "payments-api", port: 8080, interceptedBy: "checkout-svc" },
      { name: "ledger", port: 8081 },
    ],
  },
  {
    id: "e6",
    name: "ml-serving",
    owner: "arjun",
    services: [
      { name: "inference", port: 9000 },
      { name: "feature-store", port: 6566 },
      { name: "redis", port: 6379 },
    ],
  },
  {
    id: "e7",
    name: "staging",
    owner: "devops",
    services: [
      { name: "postgres", port: 5432 },
      { name: "api", port: 8080 },
      { name: "grafana", port: 3000 },
    ],
  },
];

//! Test fixture only (never imported by production code): the demo workspaces and environments the
//! sidebar once showed, kept so app.test.tsx can feed them through the backend `space()` seam.
import type { SpaceView } from "@kloudlite-tui/backend";
import type { Environment, Workspace } from "./workspaces.ts";

/** The working session's workspaces — they move with it, never with an environment. */
export const MOCK_WORKSPACES: Workspace[] = [
  { id: "w1", name: "api-gateway", owner: "karthik", status: "attached", ports: [8080, 9090], repo: "kloudlite/api-gateway", branch: "feat/rate-limits", changes: 12, processes: [
    { id: "p1", name: "server", command: "gateway serve --port 8080", status: "running", startedAt: "2026-10-09T08:00:00Z", logs: [
      { text: "listening on :8080" },
      { text: "route  GET  /healthz" },
      { text: "route  POST /v1/tokens" },
      { text: "rate limiter: 100 req/s per key" },
      { text: "GET /healthz 200 1.2ms" },
      { text: "POST /v1/tokens 201 18.4ms" },
    ] },
    { id: "p2", name: "metrics", command: "metrics serve --listen :9090", status: "running", startedAt: "2026-10-09T08:05:00Z", logs: [
      { text: "serving /metrics on :9090" },
      { text: "scrape from 10.0.2.14 200 0.8ms" },
    ] },
    { id: "p3", name: "tests", command: "go test ./... -watch", status: "crashed", code: 1, startedAt: "2026-10-09T08:10:00Z", logs: [
      { text: "=== RUN   TestRateLimit" },
      { text: "--- FAIL: TestRateLimit (0.03s)", err: true },
      { text: "    limiter_test.go:42: want 100 got 128", err: true },
      { text: "FAIL  github.com/kloudlite/api-gateway/limiter" },
      { text: "exit status 1" },
    ] },
  ] },
  { id: "w1a", name: "rate-limits-probe", owner: "karthik", parent: "w1", task: "Filter the OwnerBinding watch to owned namespaces", status: "running", ports: [8090], repo: "kloudlite/api-gateway", branch: "feat/rate-limits-probe", changes: 3, processes: [
    { id: "p1", name: "probe", command: "probe watch api-gateway:8080", status: "running", startedAt: "2026-10-09T07:30:00Z", logs: [
      { text: "probing api-gateway:8080 every 5s" },
      { text: "p99 latency 42ms over 120 samples" },
    ] },
  ] },
  { id: "w1b", name: "load-test", owner: "karthik", parent: "w1", task: "Review bc5a5062 against the rate-limit spec", status: "stopped", ports: [], repo: "kloudlite/api-gateway", branch: "feat/rate-limits-probe", changes: 0 },
  { id: "w2", name: "billing-svc", owner: "karthik", status: "running", ports: [8081], repo: "kloudlite/billing-svc", branch: "main", changes: 5, processes: [
    { id: "p1", name: "server", command: "bun run dev", status: "running", startedAt: "2026-10-07T05:00:00Z", logs: [
      { text: "bun v1.3.14 ready in 41ms" },
      { text: "listening on http://localhost:8081" },
    ] },
    { id: "p2", name: "migrate", command: "bun run migrate", status: "exited", code: 0, startedAt: "2026-10-09T08:20:00Z", logs: [
      { text: "applied 3 migrations" },
      { text: "schema at revision 2026_08_31_a" },
    ] },
  ] },
  { id: "w3", name: "console-web", owner: "karthik", status: "cloning", progress: "42%", ports: [], repo: "kloudlite/console-web", branch: "main", changes: 0, processes: [
    { id: "p1", name: "vite", command: "bun run dev", status: "starting", startedAt: "2026-10-09T08:25:00Z", logs: [
      { text: "installing dependencies…" },
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

/** The mock above as the backend would report it, for tests that boot the App on a fake `space()`. */
export function fixtureSpace(): SpaceView {
  const id = new Map(MOCK_WORKSPACES.map((w) => [w.name, w.id]));
  return {
    available: true,
    user: "karthik",
    connected: MOCK_ENVIRONMENTS[0]!.id,
    workspaces: MOCK_WORKSPACES.map((w) => ({
      id: w.id,
      name: w.name,
      owner: w.owner,
      state: w.status === "stopped" ? "stopped" : w.status === "cloning" ? "creating" : "ready",
      repo: w.repo,
      branch: w.branch,
      attached_environment: w.status === "attached" ? "e1" : undefined,
      parent: w.parent,
      task: w.task,
      changes: w.changes,
      processes: w.processes?.map((p) => ({
        id: p.id,
        cmd: p.command,
        started_at: p.startedAt,
        state: p.status === "exited" || p.status === "crashed" ? "exited" : "running",
        exit_code: p.code,
        failed: p.status === "crashed",
        logs: p.logs,
      })),
    })),
    environments: MOCK_ENVIRONMENTS.map((e) => ({
      id: e.id,
      name: e.name,
      owner: e.owner,
      state: "running",
      services: e.services.map((s) => ({ name: s.name, ports: [s.port], interceptedBy: s.interceptedBy && id.get(s.interceptedBy) })),
    })),
  };
}

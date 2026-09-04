/** Prometheus text metrics for the web tier, kept in memory.
 *
 *  `prom-client` is not installed, and a registry of three counters and one histogram is less
 *  code than the wiring to add a dependency for it. Everything is per-process and lost on
 *  restart, which is what a Prometheus counter already assumes.
 *
 *  The registry hangs off `globalThis` on purpose: Next compiles `instrumentation.ts`, the route
 *  handlers and the server components into SEPARATE bundles, so a module-level `Map` would give
 *  the recorder and `/api/metrics` two different registries and the scrape would always read
 *  zeroes. `globalThis` is one object per process, which is the scope the numbers actually have.
 */

type Labels = Record<string, string>;

/** Seconds. Web pages, not batch jobs: the interesting range is 10 ms to a few seconds. */
const BUCKETS = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10];

const HELP: Record<string, string> = {
  http_requests_total: "Requests served by this process.",
  auth_failures_total: "Sign-in attempts refused.",
  upstream_requests_total: "Calls to the api and admin processes.",
};

type Registry = {
  counters: Map<string, number>;
  buckets: number[];
  sum: number;
  count: number;
};

const store: Registry = ((globalThis as { __webMetrics?: Registry }).__webMetrics ??= {
  counters: new Map(),
  buckets: new Array(BUCKETS.length).fill(0),
  sum: 0,
  count: 0,
});

/** A label value is somebody else's string (a method, an upstream status). Escaped per the
 *  exposition format, or one `"` turns the whole scrape into a parse error. */
function escape(v: string): string {
  return v.replace(/\\/g, "\\\\").replace(/\n/g, "\\n").replace(/"/g, '\\"');
}

function series(name: string, labels: Labels): string {
  const pairs = Object.keys(labels)
    .sort()
    .map((k) => `${k}="${escape(labels[k])}"`)
    .join(",");
  return pairs ? `${name}{${pairs}}` : name;
}

export function count(name: keyof typeof HELP, labels: Labels, by = 1) {
  const key = series(name, labels);
  store.counters.set(key, (store.counters.get(key) ?? 0) + by);
}

export function observe(seconds: number) {
  if (!Number.isFinite(seconds) || seconds < 0) return;
  store.sum += seconds;
  store.count += 1;
  for (let i = 0; i < BUCKETS.length; i++) if (seconds <= BUCKETS[i]) store.buckets[i] += 1;
}

/** The path, reduced to something with bounded cardinality.
 *
 *  `/{owner}/{repo}/...` is user-supplied, so the raw pathname would mint a new time series per
 *  repository and eventually per visit. Only the first segment is kept, and only when it names a
 *  section of the app rather than somebody's handle.
 */
const SECTIONS = new Set([
  "api",
  "login",
  "signup",
  "logout",
  "new",
  "settings",
  "account",
  "superadmin",
  "workspaces",
  "environments",
  "teams",
  "explore",
  "search",
  "_next",
]);

export function routeLabel(pathname: string): string {
  const first = pathname.split("/")[1] ?? "";
  if (first === "") return "/";
  return SECTIONS.has(first) ? `/${first}` : "/:owner";
}

/** The whole registry in the Prometheus text exposition format. */
export function render(): string {
  const out: string[] = [];
  for (const name of Object.keys(HELP)) {
    const lines = [...store.counters].filter(([k]) => k === name || k.startsWith(`${name}{`));
    if (lines.length === 0) continue;
    out.push(`# HELP ${name} ${HELP[name]}`, `# TYPE ${name} counter`);
    for (const [k, v] of lines) out.push(`${k} ${v}`);
  }
  out.push(
    "# HELP http_request_duration_seconds Time to serve a request.",
    "# TYPE http_request_duration_seconds histogram",
  );
  for (let i = 0; i < BUCKETS.length; i++) {
    out.push(`http_request_duration_seconds_bucket{le="${BUCKETS[i]}"} ${store.buckets[i]}`);
  }
  out.push(
    `http_request_duration_seconds_bucket{le="+Inf"} ${store.count}`,
    `http_request_duration_seconds_sum ${store.sum}`,
    `http_request_duration_seconds_count ${store.count}`,
  );
  return `${out.join("\n")}\n`;
}

/** Whether a request for `/api/metrics` arrived on the pod's own address.
 *
 *  The same shape of gate `/api/health` relies on: the probe and the scrape reach the container
 *  port directly, so they need no credential, while anything from the public hostname has come
 *  through the ingress and is refused. The signal is the `Host` header, which the collector sends
 *  as `{podIP}:{PORT}` and the ingress sends as the public name with no port at all.
 *
 *  Not `x-forwarded-*`: Next fills those in itself when they are absent, so they are present on
 *  every request and say nothing about who sent it. The public host is refused by name as well,
 *  in case something ever forwards `Host` through verbatim.
 */
export function scrapeAllowed(headers: Headers): boolean {
  const host = (headers.get("host") ?? "").toLowerCase();
  const port = host.includes(":") ? host.slice(host.lastIndexOf(":") + 1) : "";
  if (port !== (process.env.PORT ?? "3000")) return false;
  const name = host.slice(0, host.length - port.length - 1);
  return name !== publicHost();
}

function publicHost(): string {
  const url = process.env.AUTH_URL ?? "";
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return "";
  }
}

/** Only for tests: a fresh registry, so one test's counts are not another's. */
export function reset() {
  store.counters.clear();
  store.buckets.fill(0);
  store.sum = 0;
  store.count = 0;
}

import { log } from "@/lib/log";
import { count, observe, routeLabel } from "@/lib/metrics";

const logger = log("web::instrumentation");

/** The same floor the Rust tiers' `http.slow` uses. */
const SLOW_PAGE_MS = 1_000;

/** Per request, read by `lib/api/client.ts` through `globalThis` (separate bundles, one process). */
type RequestTiming = { reqId: string; upstreamMs: number; upstreamCalls: number };

/** Where `http_requests_total` and the duration histogram are actually recorded.
 *
 *  Next 16's `proxy.ts` (the renamed middleware) runs BEFORE the route is rendered and never sees
 *  the response, so it can report neither the status nor the time taken — the two things the
 *  metrics are for. `register()` is the framework's own hook for observability wiring and runs in
 *  the server process before it serves anything, so the one place with both numbers is the
 *  response itself. Patching the prototype rather than a server instance is what avoids a custom
 *  server: `next start` creates its listener after this has run.
 *
 *  ponytail: one process-wide patch, applied once and never removed. If the web tier ever needs
 *  traces as well as counts, this is the seam an OpenTelemetry Node SDK registration replaces
 *  wholesale — its http instrumentation does exactly this, correctly, for far more code.
 */
export async function registerNode() {
  const http = await import("node:http");
  const proto = http.Server.prototype as unknown as {
    __webMetricsPatched?: boolean;
    emit: (event: string, ...args: unknown[]) => boolean;
  };
  if (proto.__webMetricsPatched) return;
  proto.__webMetricsPatched = true;

  const { AsyncLocalStorage } = await import("node:async_hooks");
  const timing = new AsyncLocalStorage<RequestTiming>();
  (globalThis as { __webRequestTiming?: typeof timing }).__webRequestTiming = timing;

  const emit = proto.emit;
  proto.emit = function (this: unknown, event: string, ...args: unknown[]) {
    if (event === "request") {
      const req = args[0] as { url?: string; method?: string; headers?: Record<string, string | string[] | undefined> };
      const res = args[1] as { statusCode: number; on: (e: string, f: () => void) => void };
      const started = performance.now();
      const header = req.headers?.["x-request-id"];
      const store: RequestTiming = {
        reqId: typeof header === "string" && /^[A-Za-z0-9_.:-]{1,128}$/.test(header) ? header : "",
        upstreamMs: 0,
        upstreamCalls: 0,
      };
      // `close` fires for an aborted request too, which is exactly the one a latency histogram
      // must not silently drop.
      res.on("close", () => {
        try {
          const pathname = new URL(req.url ?? "/", "http://x").pathname;
          const route = routeLabel(pathname);
          const ms = performance.now() - started;
          observe(ms / 1000);
          count("http_requests_total", {
            route,
            method: req.method ?? "GET",
            status: String(res.statusCode),
          });
          // The page's own time against what it spent waiting on the api: the split a slow
          // `web.pages` sample needs, which the histogram cannot give per request.
          if (ms > SLOW_PAGE_MS && !pathname.startsWith("/_next/")) {
            logger.warn("page.slow", {
              path: pathname,
              method: req.method ?? "GET",
              status: res.statusCode,
              ms: Math.round(ms),
              upstream_ms: Math.round(store.upstreamMs),
              upstream_calls: store.upstreamCalls,
              req_id: store.reqId,
            });
          }
        } catch {
          // A metric is never worth failing a request that already succeeded.
        }
      });
      return timing.run(store, () => emit.apply(this, [event, ...args]));
    }
    return emit.apply(this, [event, ...args]);
  };

  logger.info("web.metrics.installed");
}


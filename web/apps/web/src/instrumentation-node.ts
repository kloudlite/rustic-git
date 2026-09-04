import { log } from "@/lib/log";
import { count, observe, routeLabel } from "@/lib/metrics";

const logger = log("web::instrumentation");

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

  const emit = proto.emit;
  proto.emit = function (this: unknown, event: string, ...args: unknown[]) {
    if (event === "request") {
      const req = args[0] as { url?: string; method?: string };
      const res = args[1] as { statusCode: number; on: (e: string, f: () => void) => void };
      const started = performance.now();
      // `close` fires for an aborted request too, which is exactly the one a latency histogram
      // must not silently drop.
      res.on("close", () => {
        try {
          const route = routeLabel(new URL(req.url ?? "/", "http://x").pathname);
          observe((performance.now() - started) / 1000);
          count("http_requests_total", {
            route,
            method: req.method ?? "GET",
            status: String(res.statusCode),
          });
        } catch {
          // A metric is never worth failing a request that already succeeded.
        }
      });
    }
    return emit.apply(this, [event, ...args]);
  };

  logger.info("web.metrics.installed");
}


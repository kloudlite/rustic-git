import { log, reason } from "@/lib/log";

const logger = log("web::instrumentation");

/** Metrics recording is Node-only and lives in its own module: `instrumentation.ts` is compiled
 *  for the Edge runtime as well, and a `node:http` import in this file is bundled (and warned
 *  about) even inside a branch that can never run there. Splitting it is the documented shape —
 *  see `node_modules/next/dist/docs/.../instrumentation.md`, "Specifying the runtime". */
export async function register() {
  if (process.env.NEXT_RUNTIME !== "nodejs") return;
  const { registerNode } = await import("./instrumentation-node");
  await registerNode();
}

/** Every server-side render or handler that threw, as one event. Next reports these and then
 *  renders the error boundary, so without this the only trace of a 500 is the boundary's own
 *  client-side line — which never reaches the pod's stderr. */
export function onRequestError(
  err: unknown,
  request: { path?: string; method?: string },
  context: { routerKind?: string; routePath?: string },
) {
  logger.error("page.render.failed", {
    path: request.path,
    method: request.method,
    route: context.routePath,
    router: context.routerKind,
    error: reason(err),
  });
}

import type { ApiInterceptPort, ApiService } from "./api";

/** What an environment WISHES for one service — never what is in force.
 *
 *  The two are different things and the page must not conflate them: an intercept can sit in
 *  `intercepts` while `status`'s `intercepted_by` is null, which means the workspace is stopped
 *  or unreachable and the REAL service is the one answering. `heldBy` here is the wish's
 *  workspace; whether traffic actually goes there is `ApiService.intercepted_by`, read directly.
 *
 *  `ports` is the SERVICE's declared list, in its own order, each carrying the workspace port
 *  that answers it — a port the wish does not map is answered on the same number, exactly as the
 *  controller renders it. Empty when there is no wish: there is nothing to describe. */
export function interceptSummary(
  service: ApiService,
  intercepts: { service: string; workspace: string; ports: ApiInterceptPort[] }[] | undefined,
): { heldBy: string | null; ports: ApiInterceptPort[] } {
  const wish = intercepts?.find((i) => i.service === service.name);
  if (!wish) return { heldBy: null, ports: [] };
  return {
    heldBy: wish.workspace,
    ports: service.ports.map((p) => ({
      service: p,
      workspace: wish.ports.find((m) => m.service === p)?.workspace ?? p,
    })),
  };
}

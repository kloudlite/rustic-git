import { render, scrapeAllowed } from "@/lib/metrics";

/* The scrape target. Gated the way /api/health is reachable — on the container port only, so it
   needs no credential on the pod network and is not published by the ingress. A 404 rather than a
   403: whether this process exposes metrics is not something the internet gets to learn. */
export function GET(request: Request) {
  if (!scrapeAllowed(request.headers)) return new Response(null, { status: 404 });
  return new Response(render(), {
    headers: { "content-type": "text/plain; version=0.0.4; charset=utf-8", "cache-control": "no-store" },
  });
}

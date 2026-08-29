import type { NextConfig } from "next";

/* Every response, every path. Nothing here restricts scripts or styles — a full CSP is its own
   project — but this much is free: the approve and delete buttons cannot be framed, an invite
   or verify URL (a bearer token in the path) is not handed to the next site as a Referer, and
   HSTS is asserted by the app rather than left to whatever the proxy in front happens to do. */
const SECURITY_HEADERS = [
  { key: "X-Frame-Options", value: "DENY" },
  { key: "X-Content-Type-Options", value: "nosniff" },
  { key: "Referrer-Policy", value: "strict-origin-when-cross-origin" },
  { key: "Strict-Transport-Security", value: "max-age=31536000; includeSubDomains" },
  { key: "Content-Security-Policy", value: "frame-ancestors 'none'; object-src 'none'; base-uri 'self'" },
];

const nextConfig: NextConfig = {
  /* Standalone: `next build` emits a self-contained server under .next/standalone,
     so the runtime image carries the app and its runtime deps, not the toolchain. */
  output: "standalone",
  poweredByHeader: false,
  headers: async () => [{ source: "/(.*)", headers: SECURITY_HEADERS }],
  experimental: {
    // The radix-ui monopackage re-exports everything; without this, one import
    // pulls the whole barrel into every chunk that touches a UI primitive.
    optimizePackageImports: ["radix-ui"],
    /* Every page here is dynamic (session, `cache: "no-store"` reads), and Next's client cache
       keeps a dynamic segment for 0 seconds by default. So moving between a subject's tabs —
       Live services ↔ Snapshots, Code ↔ Pull requests — re-fetched the page segment from the
       server EVERY time, even switching straight back to the tab just left: skeleton, height
       jump, content. Thirty seconds makes a return trip instant, and it cannot serve anything
       staler than that because `AutoRefresh` calls `router.refresh()` every 10s, which drops
       the whole client cache. Layouts are unaffected — partial rendering never refetches them. */
    staleTimes: { dynamic: 30 },
  },
};

export default nextConfig;

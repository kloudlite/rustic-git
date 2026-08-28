import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  /* Standalone: `next build` emits a self-contained server under .next/standalone,
     so the runtime image carries the app and its runtime deps, not the toolchain. */
  output: "standalone",
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

import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  /* Standalone: `next build` emits a self-contained server under .next/standalone,
     so the runtime image carries the app and its runtime deps, not the toolchain. */
  output: "standalone",
  experimental: {
    // The radix-ui monopackage re-exports everything; without this, one import
    // pulls the whole barrel into every chunk that touches a UI primitive.
    optimizePackageImports: ["radix-ui"],
  },
};

export default nextConfig;

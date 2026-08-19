import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  /* Standalone: `next build` emits a self-contained server under .next/standalone,
     so the runtime image carries the app and its runtime deps, not the toolchain. */
  output: "standalone",
};

export default nextConfig;

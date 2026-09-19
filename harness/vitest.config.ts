import { defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";

export default defineConfig({
  plugins: [solid()],
  test: {
    environment: "happy-dom",
    globals: false,
    pool: "vmThreads",
    poolOptions: { vmThreads: { singleThread: true } },
  },
});

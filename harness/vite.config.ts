import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import solid from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";

/** A path inside the vendored opencode tree. */
const oc = (rest: string) => fileURLToPath(new URL(`./src/renderer/opencode/${rest}`, import.meta.url));

export default defineConfig({
  root: "src/renderer",
  base: "./",
  plugins: [solid(), tailwindcss()],
  /**
   * opencode's session renderer is vendored under `src/renderer/opencode/` (see its VENDORED.md).
   * It imports itself by package name, so the names resolve into the vendored tree and nothing
   * outside that directory ever learns them. `@opencode-ai/client` is a shim: the real one is a
   * tarball in their repo, and we only ever needed a type from it.
   */
  resolve: {
    alias: [
      { find: /^@opencode-ai\/session-ui$/, replacement: oc("session-ui/components") },
      { find: /^@opencode-ai\/session-ui\/v2\/prompt-input(.*)$/, replacement: oc("session-ui/v2/components/prompt-input$1") },
      { find: /^@opencode-ai\/session-ui\/v2\/(.*)$/, replacement: oc("session-ui/v2/components/$1") },
      { find: /^@opencode-ai\/session-ui\/styles$/, replacement: oc("session-ui/styles/index.css") },
      { find: /^@opencode-ai\/session-ui\/pierre(.*)$/, replacement: oc("session-ui/pierre$1") },
      { find: /^@opencode-ai\/session-ui\/context(.*)$/, replacement: oc("session-ui/context$1") },
      { find: /^@opencode-ai\/session-ui\/(.*)$/, replacement: oc("session-ui/components/$1") },
      { find: /^@opencode-ai\/ui\/styles\/tailwind$/, replacement: oc("ui/styles/tailwind/index.css") },
      { find: /^@opencode-ai\/ui\/styles$/, replacement: oc("ui/styles/index.css") },
      { find: /^@opencode-ai\/ui\/theme(.*)$/, replacement: oc("ui/theme$1") },
      { find: /^@opencode-ai\/ui\/context(.*)$/, replacement: oc("ui/context$1") },
      { find: /^@opencode-ai\/ui\/hooks$/, replacement: oc("ui/hooks/index.ts") },
      { find: /^@opencode-ai\/ui\/i18n\/(.*)$/, replacement: oc("ui/i18n/$1") },
      { find: /^@opencode-ai\/ui\/icons\/provider$/, replacement: oc("ui/components/provider-icons/types.ts") },
      { find: /^@opencode-ai\/ui\/icons\/file-type$/, replacement: oc("ui/components/file-icons/types.ts") },
      { find: /^@opencode-ai\/ui\/icons\/app$/, replacement: oc("ui/components/app-icons/types.ts") },
      { find: /^@opencode-ai\/ui\/v2\/(.*)$/, replacement: oc("ui/v2/components/$1") },
      { find: /^@opencode-ai\/ui\/(.*)$/, replacement: oc("ui/components/$1") },
      { find: /^@opencode-ai\/core\/util\/(.*)$/, replacement: oc("core/util/$1") },
      { find: /^@opencode-ai\/sdk\/v2\/client$/, replacement: oc("sdk/v2/types.ts") },
      { find: /^@opencode-ai\/sdk\/v2(.*)$/, replacement: oc("sdk/v2/types.ts") },
      { find: /^@opencode-ai\/client\/promise$/, replacement: oc("shims/client-promise.ts") },
    ],
  },
  // The vendored renderer ships module workers (markdown, shiki); rollup refuses to code-split an
  // IIFE worker, which is vite's default.
  worker: { format: "es" },
  build: {
    outDir: "../../dist/renderer",
    emptyOutDir: true,
    rollupOptions: { input: { index: "src/renderer/index.html", preview: "src/renderer/preview.html" } },
  },
});

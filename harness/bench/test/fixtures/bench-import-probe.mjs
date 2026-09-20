import { registerHooks } from "node:module";
const seen = [];
registerHooks({ resolve(specifier, context, nextResolve) { const r = nextResolve(specifier, context); seen.push(r.url); return r; } });
await import(new URL("../../src/bench.ts", import.meta.url).href);
const hit = (needle) => seen.filter((u) => u.includes(needle)).length;
console.log(JSON.stringify({ piKloudlite: hit("/pi/kloudlite.ts"), piCatalog: hit("/pi/catalog.ts"), typebox: hit("typebox"), capabilities: hit("/operations/capabilities.ts"), adapters: hit("/operations/adapters.ts") }));
process.exit(0);

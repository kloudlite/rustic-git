import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";

/**
 * A package imported at runtime under bench/src or pi has to be a real dependency: the bench
 * image installs with `npm ci --omit=dev`, so a devDependency that only happens to be on disk
 * (hoisted by another package) works on a laptop and crashes at import in the fleet.
 */
const ROOT = path.resolve(import.meta.dirname, "..", "..");

function walk(dir: string): string[] {
  const out: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(full));
    else if (/\.(ts|tsx|mjs|js)$/.test(entry.name)) out.push(full);
  }
  return out;
}

function bareSpecifiers(files: string[]): Set<string> {
  const specifiers = new Set<string>();
  // Only real import/export/require statements, anchored at the start of a (trimmed) line, so
  // ordinary code text such as `from "${snapshot.state}"` inside a thrown message is never read
  // as a module specifier.
  const re = /^\s*(?:import\s[^;]*?\bfrom\s+["']([^"']+)["']|import\s+["']([^"']+)["']|export\s[^;]*?\bfrom\s+["']([^"']+)["'])|require\(\s*["']([^"']+)["']\s*\)/;
  for (const file of files) {
    const text = fs.readFileSync(file, "utf8");
    for (const line of text.split("\n")) {
      const match = re.exec(line);
      if (!match) continue;
      const spec = match[1] ?? match[2] ?? match[3] ?? match[4];
      if (!spec || spec.startsWith(".") || spec.startsWith("/") || spec.startsWith("node:")) continue;
      // A bare specifier's package name is the first segment, or the first two for a scope.
      const parts = spec.split("/");
      const name = spec.startsWith("@") ? parts.slice(0, 2).join("/") : parts[0];
      specifiers.add(name);
    }
  }
  return specifiers;
}

test("every package imported at runtime under bench/src and pi is a declared dependency", () => {
  const pkg = JSON.parse(fs.readFileSync(path.join(ROOT, "package.json"), "utf8")) as { dependencies?: Record<string, string> };
  const deps = new Set(Object.keys(pkg.dependencies ?? {}));
  const files = [...walk(path.join(ROOT, "bench", "src")), ...walk(path.join(ROOT, "pi"))];
  const used = bareSpecifiers(files);
  const missing = [...used].filter((name) => !deps.has(name)).sort();
  assert.deepEqual(missing, [], `imported at runtime but not in dependencies: ${missing.join(", ")}`);
});

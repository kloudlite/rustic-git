import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/**
 * A terminal has one cell (owner, 2026-09-17: "for a terminal all font sizes will be same; only
 * color and weight vary"). Nothing in the centre pane may set a size of its own — difference is
 * carried by weight, colour and case. This greps for the ways a size creeps back in.
 */
const PANE = [
  "src/renderer/components/Chat.tsx",
  "src/renderer/components/ToolCall.tsx",
  ...fs.readdirSync("src/renderer/components/results").filter((f) => f.endsWith(".tsx")).map((f) => `src/renderer/components/results/${f}`),
];
const FORBIDDEN = [/\btext-(2xs|xs|sm|base|md|lg|xl)\b/, /\btext-\[\d+px\]/, /font-size\s*:/, /\bleading-\[[\d.]+px\]/];

test("nothing in the centre pane sets its own font size", () => {
  for (const f of PANE) {
    const src = fs.readFileSync(path.resolve(f), "utf8");
    for (const re of FORBIDDEN) {
      const hit = re.exec(src);
      assert.equal(hit, null, `${f}: ${hit?.[0]} — the pane is one cell; use weight, colour or case`);
    }
  }
});

test("the cell is a token, and .prose is set in it", () => {
  const css = fs.readFileSync(path.resolve("src/renderer/styles/app.css"), "utf8");
  assert.match(css, /--cell: 13px;/);
  assert.match(css, /--cell-lh: 20px;/);
  // Every rule under .prose that sets a size sets it to the token.
  for (const m of css.matchAll(/\.prose[^{]*\{([^}]*)\}/g)) {
    const size = /font-size:\s*([^;]+);/.exec(m[1]);
    if (size) assert.match(size[1].trim(), /var\(--cell\)/, `.prose rule sets ${size[1]}`);
  }
  // A heading is bold, not bigger.
  assert.match(css, /\.prose h1, \.prose h2, \.prose h3, \.prose h4 \{ font-size: var\(--cell\)/);
});

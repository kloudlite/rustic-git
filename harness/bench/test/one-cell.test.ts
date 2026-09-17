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

/**
 * opencode's One Dark, transcribed (`one-dark.json`). These are the values a diff is drawn with,
 * so a hunk in the bench and a hunk in opencode are the same green — checked against the source
 * rather than trusted to a hand copy.
 */
test("the one-dark diff palette is transcribed exactly, both themes", () => {
  const css = fs.readFileSync(path.resolve("src/renderer/styles/app.css"), "utf8");
  for (const [name, dark, light] of [
    ["diff-added-bg", "#2c382b", "#eafbe9"],
    ["diff-removed-bg", "#3a2d2f", "#fce9e8"],
    ["diff-highlight-added", "#aad482", "#489447"],
    ["diff-highlight-removed", "#e8828b", "#d65145"],
    ["diff-line-number", "#9398a2", "#666666"],
    ["diff-added-line-number-bg", "#283427", "#e1f3df"],
    ["diff-removed-line-number-bg", "#36292b", "#f5e2e1"],
    ["diff-hunk-header", "#56b6c2", "#0184bc"],
  ] as const) {
    assert.ok(css.includes(`--${name}: ${dark};`), `${name} is missing its One Dark value`);
    assert.ok(css.includes(`--${name}: ${light};`), `${name} is missing its One Light value`);
  }
});

test("every duration this pane animates at is a token", () => {
  const css = fs.readFileSync(path.resolve("src/renderer/styles/app.css"), "utf8");
  for (const [name, value] of [
    ["--motion-body", "350ms"],
    ["--motion-shell", "320ms"],
    ["--motion-shimmer", "1200ms"],
    ["--motion-progress", "1200ms"],
    ["--motion-copied", "2000ms"],
  ]) assert.ok(css.includes(`${name}: ${value};`), `${name} is missing`);
  // An infinite animation must never be left to run at 1ms: that is a strobe, not less motion.
  assert.match(css, /animation-iteration-count: 1 !important;/);
});

/**
 * ONE status row under the composer. The owner saw the same `Build · model` twice — a turn footer
 * hard against the composer and the composer's own row — and two statements of one fact read as
 * two facts (2026-09-17, side by side with opencode).
 */
test("the composer carries exactly one status row, and the footer says where you are", () => {
  const src = fs.readFileSync(path.resolve("src/renderer/components/Chat.tsx"), "utf8");
  assert.equal(src.split('data-slot="composer-status"').length - 1, 1);
  // The idle footer names the path, not the mode again.
  assert.match(src, /fallback=\{<span class="min-w-0 truncate" title=\{thread\(\)\?\.name\}>\{where\(\)\}<\/span>\}/);
  // opencode's own composer padding (`session-composer-region.tsx:102`), and the theme's accent rail.
  assert.match(src, /border-l-2 border-accent bg-input/);
  assert.match(src, /class="flex items-start px-4 pt-3 pb-1\.5 font-mono"/);
});

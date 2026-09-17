import { strict as assert } from "node:assert";
import { test } from "node:test";
import { TEXT_RENDER_IMMEDIATE, next, paced, step } from "../../src/renderer/components/results/paced.ts";
import { mentions, typeLabel } from "../../src/renderer/components/results/mentions.ts";

test("pacing steps by size", () => {
  assert.equal(step(10), 2);
  assert.equal(step(40), 4);
  assert.equal(step(90), 8);
  assert.equal(step(400), 100);
  assert.equal(step(10000), 256);
});

test("a tick lands on a word boundary", () => {
  // step(8) === 2, then up to eight further characters to the next boundary.
  assert.equal(next("one two ", 0), 4);
  // Nothing to snap to inside the window: the plain step stands.
  assert.equal(next("abcdefghijklmnopqrstuvwxyz", 0), 4);
});

test("only plain forward growth is paced", () => {
  const long = "x".repeat(TEXT_RENDER_IMMEDIATE + 100);
  assert.equal(paced("same", "same", true), undefined);
  assert.equal(paced(long, "", false), long, "a finished message lands whole");
  assert.equal(paced("rewritten", "other", true), "rewritten", "a rewrite lands whole");
  assert.equal(paced("ab", "abcd", true), "ab", "a shrink lands whole");
  assert.equal(paced("hello there", "hello", true), "hello there", "a small burst lands whole");
  const grown = paced(long, "", true)!;
  assert.ok(grown.length > 0 && grown.length < long.length, "a big burst is paced");
});

test("a prompt's mentions are files or agents", () => {
  assert.deepEqual(mentions("look at @bins/agent/src/lib.rs now"), [
    { type: "text", text: "look at " },
    { type: "file", text: "@bins/agent/src/lib.rs" },
    { type: "text", text: " now" },
  ]);
  assert.deepEqual(mentions("@svelte take it"), [
    { type: "agent", text: "@svelte" },
    { type: "text", text: " take it" },
  ]);
  assert.deepEqual(mentions("mail karthik@kloudlite.io"), [{ type: "text", text: "mail karthik@kloudlite.io" }]);
  assert.deepEqual(mentions("plain"), [{ type: "text", text: "plain" }]);
});

test("an attachment is named by its type", () => {
  assert.equal(typeLabel("image/png"), "Image");
  assert.equal(typeLabel("application/pdf"), "PDF");
  assert.equal(typeLabel(undefined), "File");
});

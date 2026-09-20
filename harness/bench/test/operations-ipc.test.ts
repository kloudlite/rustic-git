import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { isMainFrame, operationStepId, NOT_MAIN_FRAME } from "../../src/operations-ipc.ts";

test("isMainFrame accepts only the exact main-frame handle", () => {
  const mainFrame = { id: "main" };
  assert.equal(isMainFrame(mainFrame, mainFrame), true);
});

test("isMainFrame refuses a sub-frame", () => {
  const mainFrame = { id: "main" };
  const iframe = { id: "child" };
  assert.equal(isMainFrame(iframe, mainFrame), false);
});

test("isMainFrame refuses a different webContents' main frame", () => {
  const mainFrame = { id: "main-a" };
  const otherWindowFrame = { id: "main-b" };
  assert.equal(isMainFrame(otherWindowFrame, mainFrame), false);
});

test("isMainFrame refuses when there is no main window", () => {
  const senderFrame = { id: "main" };
  assert.equal(isMainFrame(senderFrame, undefined), false);
});

test("isMainFrame refuses a null sender frame (already destroyed)", () => {
  const mainFrame = { id: "main" };
  assert.equal(isMainFrame(null, mainFrame), false);
});

test("the refusal text carries no privileged detail", () => {
  assert.equal(NOT_MAIN_FRAME, "operation controls are only accepted from the main window");
});

test("operationStepId accepts the same shape operations:decision accepts", () => {
  assert.equal(operationStepId("step-1"), "step-1");
});

test("operationStepId refuses a non-string or malformed step id", () => {
  assert.throws(() => operationStepId(123), /not a step id/);
  assert.throws(() => operationStepId(""), /not a step id/);
  assert.throws(() => operationStepId("../../etc"), /not a step id/);
});

test("the renderer CSP declares no unsafe-inline or unsafe-eval for scripts, and a CSP exists at all", () => {
  const html = readFileSync(resolve(process.cwd(), "src/renderer/index.html"), "utf8");
  const csp = /<meta\s+http-equiv="Content-Security-Policy"\s+content="([^"]+)">/.exec(html);
  assert.ok(csp, "no CSP meta tag found");
  const content = csp![1];
  // No script-src override means scripts fall under default-src; either way neither directive
  // that could execute untrusted script may carry 'unsafe-inline' or 'unsafe-eval'.
  const scriptDirective = /script-src[^;]*/.exec(content)?.[0] ?? /default-src[^;]*/.exec(content)?.[0] ?? "";
  assert.ok(scriptDirective, "neither script-src nor default-src is present");
  assert.doesNotMatch(scriptDirective, /unsafe-inline/);
  assert.doesNotMatch(scriptDirective, /unsafe-eval/);
});

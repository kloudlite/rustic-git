import assert from "node:assert/strict";
import { test } from "node:test";
import { platformResult, createPlatformAdapters } from "../src/operations/adapters.ts";
import type { PlatformCall } from "../src/operations/adapters.ts";

test("an adapter call settles when aborted", async () => {
  const controller = new AbortController();
  const call: PlatformCall = (_method, _route, _body, signal) =>
    new Promise((_resolve, reject) => {
      signal?.addEventListener("abort", () => reject(new Error("aborted")));
    });
  const pending = platformResult(call, "GET", "/v1/workspaces", undefined, false, controller.signal);
  controller.abort();
  const result = await pending;
  assert.equal(result.ok, false);
});

test("an adapter call settles on its own timeout", async () => {
  const call: PlatformCall = (_method, _route, _body, signal) =>
    new Promise((_resolve, reject) => {
      signal?.addEventListener("abort", () => reject(new Error("aborted")));
    });
  const result = await platformResult(call, "GET", "/v1/workspaces", undefined, false, undefined, 20);
  assert.equal(result.ok, false);
});

test("a 504 on a mutation is unknown, not failed", async () => {
  const call: PlatformCall = async () => ({ status: 504, data: "gateway timeout" });
  const result = await platformResult(call, "POST", "/v1/workspaces", {}, true);
  assert.equal(result.ok, false);
  if (!result.ok) assert.equal(result.error.code, "unknown_outcome");
});

test("with KL_TEAM set, an id absent from the team listing is passed through as given", async () => {
  const calls: string[] = [];
  const call: PlatformCall = async (method, route) => {
    calls.push(`${method} ${route}`);
    if (route.startsWith("/v1/workspaces?")) return { status: 200, data: [{ id: "ws-real", name: "api" }] };
    return { status: 200, data: { id: "ws-abc", packages: ["rustc"] } };
  };
  const adapters = createPlatformAdapters(call, "bench-ada", "acme");
  const result = await adapters["workspace.inspect"]({ args: { id: "ws-abc" }, states: {} });
  assert.equal(result.ok, true);
  assert.ok(calls.includes("GET /v1/workspaces?team=acme"), calls.join(", "));
  assert.ok(calls.includes("GET /v1/workspaces/ws-abc"), calls.join(", "));
});

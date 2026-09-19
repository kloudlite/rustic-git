import assert from "node:assert/strict";
import test from "node:test";
import type { OperationSource } from "../src/operations/control.ts";
import { loadOperationControl } from "../src/operations/production.ts";

const source: OperationSource = {
  inspect: async () => ({}),
  events: async () => ({}),
  cancel: async () => ({}),
  recordDecision: async () => ({}),
  provideInput: async () => ({}),
};

test("production operation control gets its source from the module and authorizes online", async () => {
  const requests: Array<{ url: string; authorization: string | null; redirect: RequestRedirect }> = [];
  const loaded = await loadOperationControl({
    api: "https://api.example.test",
    owner: "alice",
    team: "acme",
    bench: "bench-alice-acme",
    module: "fixture",
    fetch: async (input, init) => {
      requests.push({ url: String(input), authorization: new Headers(init?.headers).get("authorization"), redirect: init?.redirect ?? "follow" });
      return Response.json({ id: "bench-alice-acme", owner: "alice", team: "acme", access: "Full" });
    },
    importModule: async (name) => {
      assert.equal(name, "fixture");
      return { createOperationControl: () => ({ operationSource: source }) };
    },
  });
  assert.strictEqual(loaded.operationSource, source);
  assert.deepEqual(await loaded.operationAuthorizer?.({ authorization: "Bearer desktop", owner: "acme", login: "alice" }), { actorId: "alice", tenantId: "acme", tokenKind: "person" });
  assert.deepEqual(requests, [{ url: "https://api.example.test/v1/bench?team=acme", authorization: "Bearer desktop", redirect: "error" }]);
});

test("production operation authorizer fails closed for mismatched deployment and child credentials", async () => {
  for (const response of [
    { id: "other", owner: "alice", team: "acme", access: "Full" },
    { id: "bench-alice-acme", owner: "bob", team: "acme", access: "Full" },
    { id: "bench-alice-acme", owner: "alice", team: "other", access: "Full" },
  ]) {
    const loaded = await loadOperationControl({ api: "https://api.example.test", owner: "alice", team: "acme", bench: "bench-alice-acme", module: "fixture", fetch: async () => Response.json(response), importModule: async () => ({ createOperationControl: () => ({ operationSource: source }) }) });
    assert.equal(await loaded.operationAuthorizer?.({ authorization: "Bearer desktop", owner: "acme", login: "alice" }), undefined);
  }
  const denied = await loadOperationControl({ api: "https://api.example.test", owner: "alice", team: "acme", bench: "bench-alice-acme", module: "fixture", fetch: async () => new Response("denied", { status: 403 }), importModule: async () => ({ createOperationControl: () => ({ operationSource: source }) }) });
  assert.equal(await denied.operationAuthorizer?.({ authorization: "Bearer child-bench-tool", owner: "acme", login: "alice" }), undefined);
});

test("production operation control remains unavailable without source module or deployment identity", async () => {
  assert.deepEqual(await loadOperationControl({}), {});
  assert.deepEqual(await loadOperationControl({ module: "fixture", importModule: async () => ({ createOperationControl: () => ({ operationSource: source }) }) }), {});
});

test("malformed optional production configuration degrades to typed unavailability without secrets", async () => {
  const messages: string[] = [];
  const loaded = await loadOperationControl({
    api: "not a url with desktop-secret",
    owner: "alice",
    team: "acme",
    bench: "bench-alice-acme",
    module: "secret-module-name",
    log: (message) => messages.push(message),
  });
  assert.deepEqual(loaded, { unavailable: { code: "operation_source_unavailable", message: "operation source unavailable" } });
  assert.deepEqual(messages, ["operation control unavailable: invalid API URL"]);
  assert.doesNotMatch(messages.join(" "), /desktop-secret|secret-module-name/);
});

test("optional production module failures degrade to typed unavailability", async () => {
  const messages: string[] = [];
  const loaded = await loadOperationControl({
    api: "https://api.example.test",
    owner: "alice",
    team: "acme",
    bench: "bench-alice-acme",
    module: "secret-module-name",
    importModule: async () => { throw new Error("secret import detail"); },
    log: (message) => messages.push(message),
  });
  assert.deepEqual(loaded, { unavailable: { code: "operation_source_unavailable", message: "operation source unavailable" } });
  assert.deepEqual(messages, ["operation control unavailable: module load failed"]);
  assert.doesNotMatch(messages.join(" "), /secret/);
});

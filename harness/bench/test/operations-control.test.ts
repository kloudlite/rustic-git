import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { createBenchOperationAuthorizer, type OperationAuthorizer, type OperationPrincipal, type OperationSource } from "../src/operations/control.ts";
import type { OperationEvent, OperationSnapshot } from "../src/operations/contracts.ts";
import { DEFAULT_BUDGETS } from "../src/operations/contracts.ts";
import { FAKE } from "./fake-pi.ts";

const DIGEST = `sha256:${"a".repeat(64)}`;

function snapshot(overrides: Partial<OperationSnapshot> = {}): OperationSnapshot {
  return {
    contractVersion: "v1",
    operationId: "op-1",
    revision: 3,
    state: "running",
    createdAt: 1,
    updatedAt: 2,
    actor: { actorId: "alice", tenantId: "alice", sessionId: "s-1", turnId: "t-1" },
    scope: {},
    request: { instruction: "inspect this" },
    requestDigest: DIGEST,
    dedupeKey: "dedupe-1",
    budgets: DEFAULT_BUDGETS,
    steps: [],
    pendingDecisions: [],
    unknownOutcomes: [],
    usage: { steps: 0, selectionRounds: 0, generationCalls: 0, attempts: 0 },
    lastSequence: 1,
    ...overrides,
  };
}

const event: OperationEvent = {
  operationId: "op-1",
  sequence: 1,
  at: 2,
  phase: "progress",
  revision: 3,
  summary: "working",
};

async function up(source?: OperationSource, authorize?: OperationAuthorizer) {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-ops-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0, "127.0.0.1", undefined, undefined, { operationSource: source, operationAuthorizer: authorize });
  const request = async (method: string, route: string, value?: unknown, token = "person", identity = { owner: "alice", login: "alice" }) => {
    const response = await fetch(`http://127.0.0.1:${srv.port}${route}`, {
      method,
      headers: { authorization: `Bearer ${token}`, "x-kl-owner": identity.owner, "x-kl-login": identity.login, ...(value === undefined ? {} : { "content-type": "application/json" }) },
      body: value === undefined ? undefined : JSON.stringify(value),
    });
    return { status: response.status, body: await response.json() };
  };
  return { request, down: async () => (await bench.stop(), await srv.close()) };
}

const principal: OperationPrincipal = { actorId: "alice", tenantId: "alice", tokenKind: "person" };
const authorizer: OperationAuthorizer = async (request) => {
  if (request.authorization === "Bearer person") return principal;
  if (request.authorization === "Bearer child") return { ...principal, tokenKind: "child" };
  return undefined;
};

function source(overrides: Partial<OperationSource> = {}): OperationSource {
  return {
    inspect: async () => snapshot(),
    events: async () => ({ events: [event], nextCursor: "cursor-1", hasMore: true }),
    cancel: async () => snapshot({ revision: 4, state: "cancel_requested" }),
    recordDecision: async (_id, _decision, intent, who) => ({
      recordId: "record-1", operationId: "op-1", stepId: intent.stepId, decisionId: "decision-1",
      decisionClass: "user_authorization", actorId: who.actorId, tenantId: who.tenantId, sessionId: "s-1",
      payloadDigest: DIGEST, revision: intent.expectedRevision, policySource: "user_ui", outcome: intent.outcome, recordedAt: 2, expiresAt: 10,
    }),
    provideInput: async () => snapshot({ revision: 4, state: "running" }),
    ...overrides,
  };
}

test("operation routes explicitly report an unavailable source when production dependencies are absent", async () => {
  const t = await up();
  try {
    assert.deepEqual(await t.request("GET", "/operations/op-1"), { status: 503, body: { error: { code: "operation_source_unavailable", message: "operation source unavailable" } } });
  } finally { await t.down(); }
});

test("inspect and paginated event replay use real HTTP and return validated source payloads", async () => {
  let replay: unknown;
  const t = await up(source({ events: async (_id, cursor, limit) => (replay = { cursor, limit }, { events: [event], nextCursor: "cursor-2", hasMore: false }) }), authorizer);
  try {
    assert.deepEqual(await t.request("GET", "/operations/op-1"), { status: 200, body: snapshot() });
    assert.deepEqual(await t.request("GET", "/operations/op-1/events?after=cursor-1&limit=25"), {
      status: 200,
      body: { events: [event], nextCursor: "cursor-2", hasMore: false },
    });
    assert.deepEqual(replay, { cursor: "cursor-1", limit: 25 });
  } finally { await t.down(); }
});

test("malformed snapshots and event pages from the source are rejected", async () => {
  const badSnapshot = await up(source({ inspect: async () => ({ ...snapshot(), revision: 0 }) as OperationSnapshot }), authorizer);
  try {
    const response = await badSnapshot.request("GET", "/operations/op-1");
    assert.equal(response.status, 502);
    assert.equal(response.body.error.code, "invalid_source_payload");
  } finally { await badSnapshot.down(); }

  const badEvents = await up(source({ events: async () => ({ events: [{ ...event, sequence: 0 }] as OperationEvent[], hasMore: false }) }), authorizer);
  try {
    const response = await badEvents.request("GET", "/operations/op-1/events");
    assert.equal(response.status, 502);
    assert.equal(response.body.error.code, "invalid_source_payload");
  } finally { await badEvents.down(); }

  const foreignEvent = await up(source({ events: async () => ({ events: [{ ...event, operationId: "op-2" }], hasMore: false }) }), authorizer);
  try {
    const response = await foreignEvent.request("GET", "/operations/op-1/events");
    assert.equal(response.status, 502);
    assert.equal(response.body.error.code, "invalid_source_payload");
  } finally { await foreignEvent.down(); }
});

test("cancel, decision intent, and additional input validate bodies before calling the source", async () => {
  const calls: unknown[] = [];
  const t = await up(source({
    cancel: async (id, revision) => (calls.push(["cancel", id, revision]), snapshot({ revision: 4, state: "cancel_requested" })),
    recordDecision: async (id, decisionId, intent, who) => (calls.push(["decision", id, decisionId, intent, who]), source().recordDecision(id, decisionId, intent, who)),
    provideInput: async (id, decisionId, revision, inputs) => (calls.push(["input", id, decisionId, revision, inputs]), snapshot({ revision: 4 })),
  }), authorizer);
  try {
    assert.equal((await t.request("POST", "/operations/op-1/cancel", { expectedRevision: 3 })).status, 200);
    assert.equal((await t.request("POST", "/operations/op-1/decisions/decision-1", { stepId: "step-1", expectedRevision: 3, outcome: "granted" })).status, 200);
    assert.equal((await t.request("POST", "/operations/op-1/input", { decisionId: "decision-1", expectedRevision: 3, inputs: { region: "eu" } })).status, 200);
    assert.deepEqual(calls.map((call) => (call as unknown[])[0]), ["cancel", "decision", "input"]);
    for (const [route, body] of [
      ["/operations/op-1/cancel", { expectedRevision: 0 }],
      ["/operations/op-1/decisions/decision-1", { stepId: "step-1", expectedRevision: 3, outcome: "yes" }],
      ["/operations/op-1/input", { decisionId: "decision-1", expectedRevision: 3, inputs: { approve: true } }],
    ] as const) {
      const response = await t.request("POST", route, body);
      assert.equal(response.status, 400, route);
      assert.equal(response.body.error.code, "invalid_request", route);
    }
    assert.equal(calls.length, 3);
  } finally { await t.down(); }
});

test("bench authorizer binds verified person credentials to configured owner and headers", async () => {
  const authorize = createBenchOperationAuthorizer({
    owner: "acme",
    verify: async (token) => token === "person" ? { actorId: "alice", tokenKind: "person", teams: ["acme"] } : token === "child" ? { actorId: "alice", tokenKind: "child", teams: ["acme"] } : undefined,
  });
  assert.deepEqual(await authorize({ authorization: "Bearer person", owner: "acme", login: "alice" }), { actorId: "alice", tenantId: "acme", tokenKind: "person" });
  assert.equal(await authorize({ authorization: "Bearer person", owner: "other", login: "alice" }), undefined);
  assert.equal(await authorize({ authorization: "Bearer person", owner: "acme", login: "bob" }), undefined);
  assert.equal((await authorize({ authorization: "Bearer child", owner: "acme", login: "alice" }))?.tokenKind, "child");
  assert.equal(await authorize({ authorization: "person", owner: "acme", login: "alice" }), undefined);
});

test("typed stale revision errors propagate", async () => {
  const stale = Object.assign(new Error("revision changed"), { code: "stale_revision", expectedRevision: 3, actualRevision: 4 });
  const t = await up(source({ cancel: async () => { throw stale; } }), authorizer);
  try {
    assert.deepEqual(await t.request("POST", "/operations/op-1/cancel", { expectedRevision: 3 }), {
      status: 409,
      body: { error: { code: "stale_revision", message: "revision changed", expectedRevision: 3, actualRevision: 4 } },
    });
  } finally { await t.down(); }
});

test("mutation responses cannot switch operation or owner", async () => {
  const t = await up(source({ cancel: async () => snapshot({ operationId: "op-2", revision: 4 }) }), authorizer);
  try {
    const response = await t.request("POST", "/operations/op-1/cancel", { expectedRevision: 3 });
    assert.equal(response.status, 502);
    assert.equal(response.body.error.code, "invalid_source_payload");
  } finally { await t.down(); }
});

test("decision responses cannot switch operation or authenticated actor and tenant", async () => {
  for (const changed of [
    { operationId: "op-2" },
    { actorId: "bob" },
    { tenantId: "other" },
  ]) {
    const t = await up(source({ recordDecision: async (_id, _decision, intent, who) => ({
      recordId: "record-1", operationId: "op-1", stepId: intent.stepId, decisionId: "decision-1",
      decisionClass: "user_authorization", actorId: who.actorId, tenantId: who.tenantId, sessionId: "s-1",
      payloadDigest: DIGEST, revision: intent.expectedRevision, policySource: "user_ui", outcome: intent.outcome, recordedAt: 2, expiresAt: 10,
      ...changed,
    }) }), authorizer);
    try {
      const response = await t.request("POST", "/operations/op-1/decisions/decision-1", { stepId: "step-1", expectedRevision: 3, outcome: "granted" });
      assert.equal(response.status, 502);
      assert.equal(response.body.error.code, "invalid_source_payload");
    } finally { await t.down(); }
  }
});

test("operation routes refuse unauthenticated, child-token, and cross-owner callers", async () => {
  const t = await up(source(), authorizer);
  try {
    assert.equal((await t.request("GET", "/operations/op-1", undefined, "missing")).status, 401);
    assert.equal((await t.request("GET", "/operations/op-1", undefined, "child")).status, 403);
  } finally { await t.down(); }

  const foreign = await up(source({ inspect: async () => snapshot({ actor: { actorId: "bob", tenantId: "bob", sessionId: "s-1", turnId: "t-1" } }) }), authorizer);
  try {
    assert.equal((await foreign.request("GET", "/operations/op-1")).status, 404);
  } finally { await foreign.down(); }
});

test("operation routes validate the complete method, path, and query before auth or source access", async () => {
  let authCalls = 0;
  let sourceCalls = 0;
  const countingSource = source({ inspect: async () => (sourceCalls += 1, snapshot()) });
  const t = await up(countingSource, async () => (authCalls += 1, principal));
  try {
    for (const [method, route] of [
      ["POST", "/operations/op-1"],
      ["GET", "/operations/op-1/unknown"],
      ["GET", "/operations/op-1/cancel"],
      ["POST", "/operations/op-1/events"],
      ["GET", "/operations/op-1?extra=1"],
    ] as const) {
      assert.equal((await t.request(method, route)).status, 404, `${method} ${route}`);
    }
    assert.equal(authCalls, 0);
    assert.equal(sourceCalls, 0);
  } finally { await t.down(); }
});

test("events query accepts only one nonempty after and one limit parameter", async () => {
  let authCalls = 0;
  let sourceCalls = 0;
  const countingSource = source({ inspect: async () => (sourceCalls += 1, snapshot()) });
  const t = await up(countingSource, async () => (authCalls += 1, principal));
  try {
    for (const route of [
      "/operations/op-1/events?after=",
      "/operations/op-1/events?after=a&after=b",
      "/operations/op-1/events?limit=1&limit=2",
      "/operations/op-1/events?unknown=1",
    ]) {
      assert.equal((await t.request("GET", route)).status, 400, route);
    }
    assert.equal(authCalls, 0);
    assert.equal(sourceCalls, 0);
  } finally { await t.down(); }
});

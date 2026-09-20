import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "vitest";
import { render } from "solid-js/web";
import { createSignal } from "solid-js";
import { ToolCall } from "../../src/renderer/components/ToolCall.tsx";
import { Tasks } from "../../src/renderer/components/inspector/Tasks.tsx";
import { OperationTaskView } from "../../src/renderer/components/TaskView.tsx";
import { scenarioById } from "../../src/renderer/operations/fixtures/scenarios.ts";
import { createOperationStore, operationTaskRow } from "../../src/renderer/operations/store.ts";
import { openScenario } from "../../src/renderer/operations/fixtures/scenarios.ts";
import { sessionOf } from "../../src/renderer/rows.ts";

let dispose: (() => void) | undefined;

beforeEach(() => document.body.replaceChildren());
afterEach(() => { dispose?.(); dispose = undefined; });

async function waitFor(predicate: () => boolean) {
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if (predicate()) return;
    await Promise.resolve();
  }
  assert.fail("condition did not become true");
}

test("operate tool calls mount one live operation panel and other tools do not", async () => {
  const scenario = scenarioById("parallel-steps");
  const result = { operationId: scenario.expected.operationId, revision: 1, state: "running", summary: "Started" };
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <>
    <ToolCall a={{ role: "action", kind: "note", at: "now", text: "operate", tool: "operate", output: JSON.stringify(result) }} operations={store} />
    <ToolCall a={{ role: "action", kind: "note", at: "now", text: "read", tool: "read", output: JSON.stringify({ operationId: scenario.expected.operationId }) }} operations={store} />
  </>, host);
  await waitFor(() => host.querySelectorAll("[data-component=operation]").length === 1);
  assert.equal(store.entries().length, 1);
  store.dispose();
});

test("two ToolCalls showing one operation: unmounting the first leaves the second live; unmounting both releases it", async () => {
  const scenario = scenarioById("parallel-steps");
  const result = { operationId: scenario.expected.operationId, revision: 1, state: "running", summary: "Started" };
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  const [showFirst, setShowFirst] = createSignal(true);
  const [showSecond, setShowSecond] = createSignal(true);
  const a = { role: "action" as const, kind: "note" as const, at: "now", text: "operate", tool: "operate", output: JSON.stringify(result) };
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <>
    {showFirst() && <ToolCall a={a} operations={store} />}
    {showSecond() && <ToolCall a={a} operations={store} />}
  </>, host);
  await waitFor(() => store.entries().length === 1);
  setShowFirst(false);
  await waitFor(() => host.querySelectorAll("[data-component=operation]").length === 1);
  assert.equal(store.entries().length, 1, "the second ToolCall's own view keeps the projection held");
  setShowSecond(false);
  await waitFor(() => store.entries().length === 0);
  store.dispose();
});

test("the inspector lists an operation opened from a session tab, under the same sessionOf() keys Chat now uses", async () => {
  const scenario = scenarioById("parallel-steps");
  const result = { operationId: scenario.expected.operationId, revision: 1, state: "running", summary: "Started" };
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  // What Chat.tsx's `opSession`/`opWorkspace` compute for a workspace tab (`ws-1`), the same
  // `sessionOf({kind:"workspace", id})` shape Inspector.tsx's `procSession` computes from
  // `props.selected` for that same workspace. Before this fix Chat passed the raw pi/thread id
  // instead, which only coincidentally matched; this pins the two call sites to one function.
  const chatSessionId = sessionOf({ kind: "workspace", id: "ws-1" });
  const chatWorkspaceId = "ws-1";
  const host = document.createElement("div");
  document.body.append(host);
  const a = { role: "action" as const, kind: "note" as const, at: "now", text: "operate", tool: "operate", output: JSON.stringify(result) };
  dispose = render(() => <>
    <ToolCall a={a} operations={store} sessionId={chatSessionId} workspaceId={chatWorkspaceId} />
    <Tasks onOpen={() => undefined} session={chatSessionId} workspace={chatWorkspaceId} operations={() => { store.entries(); return store.taskRows(chatSessionId, chatWorkspaceId); }} />
  </>, host);
  await waitFor(() => host.querySelectorAll("[data-operation-id]").length === 1);
  store.dispose();
});

test("operation rows open the same projected detail without replacing legacy task rows", () => {
  const operation = operationTaskRow(openScenario(scenarioById("parallel-steps")));
  let opened: unknown;
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <>
    <Tasks onOpen={() => undefined} onOpenOperation={(row) => { opened = row; }} session="bench" workspace="ws" operations={[operation]} />
    <OperationTaskView operation={operation.projection} onClose={() => undefined} />
  </>, host);
  (host.querySelector("[data-operation-id]") as HTMLElement).click();
  assert.strictEqual(opened, operation.projection);
  assert.match(host.textContent ?? "", /Background operations/i);
  assert.equal(host.querySelectorAll("[data-component=operation]").length, 1);
});

test("taskboard reacts when operation entries are added", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  const [rows, setRows] = createSignal(store.taskRows("bench", "ws"));
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <Tasks onOpen={() => undefined} session="bench" workspace="ws" operations={rows()} />, host);
  assert.equal(host.querySelectorAll("[data-operation-id]").length, 0);
  store.open(scenario.expected.operationId, { sessionId: "bench", workspaceId: "ws" });
  setRows(store.taskRows("bench", "ws"));
  await waitFor(() => host.querySelectorAll("[data-operation-id]").length === 1);
});

test("operation rows are keyboard buttons and detail resolves the live projection with shared callbacks", async () => {
  const scenario = scenarioById("parallel-steps");
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  const projection = store.open(scenario.expected.operationId, { sessionId: scenario.expected.actor.sessionId, workspaceId: "ws" });
  await waitFor(() => projection.view().state === scenario.expected.state);
  let opened: unknown;
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <Tasks onOpen={() => undefined} onOpenOperation={(entry) => { opened = entry; }} session={scenario.expected.actor.sessionId} workspace="ws" operations={store.taskRows(scenario.expected.actor.sessionId, "ws")} />, host);
  const row = host.querySelector("[data-operation-id]") as HTMLElement;
  assert.equal(row.tagName, "BUTTON");
  row.focus();
  row.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  row.click();
  assert.strictEqual(opened, projection);
  store.dispose();
});

test("same operation ID ownership mismatch renders a typed load error instead of throwing", async () => {
  const scenario = scenarioById("parallel-steps");
  const result = { operationId: scenario.expected.operationId, revision: 1, state: "running", summary: "Started" };
  const store = createOperationStore({ loadSnapshot: async () => scenario.expected, loadEvents: async () => [] });
  store.open(result.operationId, { sessionId: "first", workspaceId: "ws" });
  const host = document.createElement("div");
  document.body.append(host);
  let thrown: unknown;
  assert.doesNotThrow(() => {
    try {
      dispose = render(() => <ToolCall a={{ role: "action", kind: "note", at: "now", text: "operate", tool: "operate", output: JSON.stringify(result) }} operations={store} sessionId="second" workspaceId="ws" />, host);
    } catch (error) {
      thrown = error;
      throw error;
    }
  });
  assert.equal(thrown, undefined);
  assert.equal(store.open(result.operationId, { sessionId: "second", workspaceId: "ws" }).error()?.code, "operation_owner_mismatch");
  store.dispose();
});

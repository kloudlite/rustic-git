import { afterEach, beforeEach, test } from "vitest";
import assert from "node:assert/strict";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { Confirm } from "../../src/renderer/ui/Confirm.tsx";
import { OperationPanel } from "../../src/renderer/operations/components/OperationPanel.tsx";
import { openScenario, scenarioById, FIXTURE_CLOCK } from "../../src/renderer/operations/fixtures/scenarios.ts";
import type { OperationView } from "../../src/renderer/operations/types.ts";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

let dispose: (() => void) | undefined;

beforeEach(() => {
  document.body.replaceChildren();
});

afterEach(() => {
  dispose?.();
  dispose = undefined;
});

function mount(view: OperationView, props: Record<string, unknown> = {}) {
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <OperationPanel view={view} now={FIXTURE_CLOCK + 2_000} {...props} />, host);
  return host;
}

function click(element: Element) {
  element.dispatchEvent(new MouseEvent("click", { bubbles: true }));
}

function key(element: Element, value: string) {
  element.dispatchEvent(new KeyboardEvent("keydown", { key: value, bubbles: true }));
}

function panelButton(host: Element): HTMLButtonElement {
  return host.querySelector(".op-panel > header button") as HTMLButtonElement;
}

const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};

async function waitFor(predicate: () => boolean, message: string) {
  for (let attempt = 0; attempt < 20; attempt += 1) {
    if (predicate()) return;
    await Promise.resolve();
  }
  assert.fail(message);
}

test("OperationPanel expansion remains controlled when expanded is supplied", () => {
  const view = openScenario(scenarioById("parallel-steps"));
  const [expanded, setExpanded] = createSignal(false);
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <OperationPanel view={view} expanded={expanded()} onToggle={setExpanded} />, host);
  const toggle = panelButton(host);
  assert.equal(toggle.getAttribute("aria-expanded"), "false");
  click(toggle);
  assert.equal(toggle.getAttribute("aria-expanded"), "true");
  setExpanded(false);
  assert.equal(toggle.getAttribute("aria-expanded"), "false");
});

test("step disclosure uses native button semantics", () => {
  const host = mount(openScenario(scenarioById("parallel-steps")), { expanded: true });
  const step = host.querySelector("[data-component=operation-step]")!;
  const toggle = step.querySelector("button")!;
  assert.equal(toggle.getAttribute("aria-expanded"), "false");
  assert.ok(toggle.getAttribute("aria-controls"));
  assert.equal(toggle.tagName, "BUTTON");
  assert.equal(toggle.type, "button");
  toggle.click();
  assert.equal(toggle.getAttribute("aria-expanded"), "true");
  assert.ok(document.getElementById(toggle.getAttribute("aria-controls")!));
  toggle.click();
  assert.equal(toggle.getAttribute("aria-expanded"), "false");
});

test("additional input has a programmatic label and submits on Enter", async () => {
  const view = openScenario(scenarioById("needs-input"));
  let sent: unknown;
  const host = mount(view, { expanded: true, onAdditionalInput: (payload: unknown) => { sent = payload; } });
  const input = host.querySelector("input") as HTMLInputElement;
  assert.ok(input.id);
  assert.ok(host.querySelector(`label[for="${input.id}"]`));
  input.value = "use 30 seconds";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.closest("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await Promise.resolve();
  assert.equal((sent as { inputs: { answer: string } }).inputs.answer, "use 30 seconds");
});

test("lifecycle and repair status are exposed as live status text", () => {
  const view = { ...openScenario(scenarioById("reconnect")), status: "repairing" as const };
  const host = mount(view);
  const statuses = [...host.querySelectorAll("[role=status]")];
  assert.ok(statuses.some((node) => node.textContent?.includes("catching up")));
  assert.ok(statuses.some((node) => node.textContent?.toLowerCase().includes("accepted")));
});

test("promise controls disable duplicate submits and expose rejection", async () => {
  const request = deferred<void>();
  let calls = 0;
  const host = mount(openScenario(scenarioById("needs-input")), {
    expanded: true,
    onAdditionalInput: () => { calls += 1; return request.promise; },
  });
  const input = host.querySelector("input") as HTMLInputElement;
  input.value = "facts";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  const send = [...host.querySelectorAll("button")].find((button) => button.textContent === "Send")! as HTMLButtonElement;
  click(send);
  click(send);
  assert.equal(calls, 1);
  assert.equal(send.disabled, true);
  request.reject(new Error("decision changed"));
  await waitFor(() => !send.disabled, "send control did not settle");
  assert.equal(send.disabled, false);
  assert.match(host.querySelector("[role=alert]")?.textContent ?? "", /decision changed/);
});

test("approval revalidates the current decision while confirmation is open", async () => {
  const [view, setView] = createSignal(openScenario(scenarioById("needs-input")));
  let calls = 0;
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <OperationPanel view={view()} now={FIXTURE_CLOCK + 2_000} expanded onDecision={() => { calls += 1; }} />, host);
  const approve = [...host.querySelectorAll("button")].find((button) => button.textContent === "Approve")!;
  click(approve);
  // Confirm renders through a Portal, under document.body, not under `host`.
  assert.ok(document.body.querySelector("[role=dialog]"));
  setView({ ...view(), pendingDecisions: [] });
  await Promise.resolve();
  const confirm = [...document.body.querySelectorAll("button")].find((button) => button.textContent === "Approve and apply") as HTMLButtonElement | undefined;
  assert.ok(!confirm || confirm.disabled, "stale confirmation must close or disable");
  if (confirm) click(confirm);
  assert.equal(calls, 0);
});

function mountConfirm(action: "cancel" | "yes" | "backdrop" | "escape" | "controlled") {
  const opener = document.createElement("button");
  opener.textContent = "open";
  document.body.append(opener);
  opener.focus();
  const [open, setOpen] = createSignal(true);
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <Confirm open={open()} title="Apply change?" body={<p>Details</p>} danger="Apply" onYes={() => action === "yes" && setOpen(false)} onNo={() => setOpen(false)} />, host);
  return { opener, host, setOpen, disposeNow: () => { dispose?.(); dispose = undefined; } };
}

for (const action of ["cancel", "yes", "backdrop", "escape", "controlled"] as const) {
  test(`Confirm restores focus after ${action}`, async () => {
    const mounted = mountConfirm(action);
    await Promise.resolve();
    const dialog = document.body.querySelector("[role=dialog]")!;
    if (action === "cancel") click([...dialog.querySelectorAll("button")].find((button) => button.textContent === "Cancel")!);
    if (action === "yes") click([...dialog.querySelectorAll("button")].find((button) => button.textContent === "Apply")!);
    if (action === "backdrop") dialog.parentElement!.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    if (action === "escape") key(dialog, "Escape");
    if (action === "controlled") mounted.setOpen(false);
    await Promise.resolve();
    assert.equal(document.activeElement, mounted.opener);
  });
}

test("Confirm does not steal focus when another modal opens during close", async () => {
  const { opener, setOpen } = mountConfirm("controlled");
  await Promise.resolve();
  const anotherModal = document.createElement("button");
  document.body.append(anotherModal);
  setOpen(false);
  anotherModal.focus();
  await Promise.resolve();
  assert.equal(document.activeElement, anotherModal);
  assert.notEqual(document.activeElement, opener);
});

test("Confirm contains document-level focus while open", async () => {
  mountConfirm("controlled");
  await Promise.resolve();
  const dialog = document.body.querySelector("[role=dialog]")!;
  const outside = document.createElement("button");
  document.body.append(outside);
  outside.focus();
  outside.dispatchEvent(new FocusEvent("focusin", { bubbles: true }));
  assert.ok(dialog.contains(document.activeElement));
});

test("Confirm enters and traps focus in both tab directions", async () => {
  mountConfirm("controlled");
  await Promise.resolve();
  const dialog = document.body.querySelector("[role=dialog]")!;
  assert.equal(dialog.getAttribute("aria-modal"), "true");
  assert.ok(dialog.getAttribute("aria-labelledby"));
  assert.equal((document.activeElement as HTMLElement).textContent, "Cancel");
  const apply = [...dialog.querySelectorAll("button")].find((button) => button.textContent === "Apply")!;
  apply.focus();
  key(dialog, "Tab");
  assert.equal((document.activeElement as HTMLElement).textContent, "Cancel");
  const cancel = [...dialog.querySelectorAll("button")].find((button) => button.textContent === "Cancel")!;
  cancel.focus();
  dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true }));
  assert.equal((document.activeElement as HTMLElement).textContent, "Apply");
});

test("Confirm renders through a portal: outside its host, inside document.body, and gone on dispose", async () => {
  const { host, disposeNow } = mountConfirm("controlled");
  await Promise.resolve();
  const dialog = document.body.querySelector("[role=dialog]")!;
  assert.equal(host.contains(dialog), false, "the dialog is still a child of its host");
  assert.equal(document.body.contains(dialog), true);
  disposeNow();
  assert.equal(document.body.querySelector("[role=dialog]"), null, "the portal left an orphaned dialog behind");
});

test("Confirm's backdrop is fixed to the viewport, not positioned relative to a containing ancestor", async () => {
  mountConfirm("controlled");
  await Promise.resolve();
  const backdrop = document.body.querySelector("[role=dialog]")!.parentElement!;
  assert.ok(backdrop.classList.contains("fixed"), "the backdrop must be fixed to escape a `contain: layout` ancestor (e.g. Chat.tsx's action rows)");
  assert.ok(!backdrop.classList.contains("absolute"));
});

test("cancel is promise-aware, prevents duplicates, and reports rejection", async () => {
  const request = deferred<void>();
  let calls = 0;
  const host = mount(openScenario(scenarioById("queued-dependencies")), {
    expanded: true,
    onCancel: () => { calls += 1; return request.promise; },
  });
  const cancel = host.querySelector('button[title="Ask the operation to stop"]') as HTMLButtonElement;
  cancel.click();
  cancel.click();
  assert.equal(calls, 1);
  assert.equal(cancel.disabled, true);
  request.reject(new Error("cancel refused"));
  await waitFor(() => !!host.querySelector("[role=alert]"), "cancel error did not render");
  assert.equal(cancel.disabled, false);
  assert.match(host.querySelector("[role=alert]")?.textContent ?? "", /cancel refused/);
});

test("cancel and resync share one visible busy state", async () => {
  const request = deferred<void>();
  const queued = openScenario(scenarioById("queued-dependencies"));
  const cancellable = { ...queued, state: "running" as const, cancellation: { requested: false, canRequest: true } };
  const view = { ...cancellable, resync: { operationId: queued.operationId, afterSequence: queued.sequence, need: "snapshot" as const, reason: "invalid_snapshot" as const, at: FIXTURE_CLOCK } };
  const host = mount(view, {
    expanded: true,
    onCancel: () => request.promise,
    onResync: () => request.promise,
  });
  const cancel = host.querySelector('button[title="Ask the operation to stop"]') as HTMLButtonElement;
  const reload = [...host.querySelectorAll("button")].find((button) => button.textContent === "Reload now")! as HTMLButtonElement;
  click(reload);
  await waitFor(() => cancel.disabled && reload.disabled, "shared controls did not enter busy state");
  request.resolve();
  await waitFor(() => !cancel.disabled && !reload.disabled, "shared controls did not settle");
});

test("resync is promise-aware, prevents duplicates, and reports rejection", async () => {
  const request = deferred<void>();
  let calls = 0;
  const host = mount(openScenario(scenarioById("reconnect")), {
    expanded: true,
    onResync: () => { calls += 1; return request.promise; },
  });
  const reload = [...host.querySelectorAll("button")].find((button) => button.textContent === "Reload now")! as HTMLButtonElement;
  click(reload);
  click(reload);
  assert.equal(calls, 1);
  assert.equal(reload.disabled, true);
  request.reject(new Error("reload failed"));
  await waitFor(() => !!host.querySelector("[role=alert]"), "reload error did not render");
  assert.equal(reload.disabled, false);
  assert.match(host.querySelector("[role=alert]")?.textContent ?? "", /reload failed/);
});

test("decision card survives an unrelated view rebuild without losing typed input or the open confirmation", async () => {
  const base = openScenario(scenarioById("needs-input"));
  const [view, setView] = createSignal(base);
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <OperationPanel view={view()} now={FIXTURE_CLOCK + 2_000} expanded />, host);
  const input = host.querySelector("input") as HTMLInputElement;
  input.value = "draft answer";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  const approve = [...host.querySelectorAll("button")].find((button) => button.textContent === "Approve")!;
  click(approve);
  assert.ok(document.body.querySelector("[role=dialog]"), "confirmation did not open");
  // An unrelated view change (a fresh object, same decision content) rebuilds `decisionRows()`
  // wholesale; a card keyed by array position or row reference would remount and lose both.
  setView({ ...view(), sequence: view().sequence });
  await Promise.resolve();
  assert.equal((host.querySelector("input") as HTMLInputElement).value, "draft answer");
  assert.ok(document.body.querySelector("[role=dialog]"), "confirmation dialog closed on an unrelated rebuild");
});

test("decision card survives ten clock ticks without losing typed input or the open confirmation", async () => {
  const base = openScenario(scenarioById("needs-input"));
  const [now, setNow] = createSignal(FIXTURE_CLOCK + 2_000);
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => <OperationPanel view={base} now={now()} expanded />, host);
  const input = host.querySelector("input") as HTMLInputElement;
  input.value = "draft answer";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  const approve = [...host.querySelectorAll("button")].find((button) => button.textContent === "Approve")!;
  click(approve);
  assert.ok(document.body.querySelector("[role=dialog]"), "confirmation did not open");
  for (let tick = 1; tick <= 10; tick += 1) {
    setNow(FIXTURE_CLOCK + 2_000 + tick * 500);
    await Promise.resolve();
  }
  assert.equal((host.querySelector("input") as HTMLInputElement).value, "draft answer");
  assert.ok(document.body.querySelector("[role=dialog]"), "confirmation dialog closed while the clock advanced");
});

test("a granted decision stops being answerable once it resolves", async () => {
  const base = openScenario(scenarioById("needs-input"));
  const [view, setView] = createSignal(base);
  const host = document.createElement("div");
  document.body.append(host);
  let resolved = false;
  dispose = render(() => <OperationPanel view={view()} now={FIXTURE_CLOCK + 2_000} expanded onDecision={() => { resolved = true; }} />, host);
  const approve = [...host.querySelectorAll("button")].find((button) => button.textContent === "Approve")!;
  click(approve);
  const confirm = [...document.body.querySelectorAll("button")].find((button) => button.textContent === "Approve and apply")!;
  click(confirm);
  assert.ok(resolved, "onDecision was not called");
  // The real store removes a granted decision from `pendingDecisions` on the next snapshot/event;
  // simulate that here to prove the card stops offering Approve once it is no longer open.
  setView({ ...view(), pendingDecisions: view().pendingDecisions.filter((d) => d.decisionId !== "dec-approve-edit") });
  await Promise.resolve();
  const stillApprove = [...host.querySelectorAll("button")].find((button) => button.textContent === "Approve");
  assert.equal(stillApprove, undefined, "a granted decision is still answerable");
});

test("responsive rules wrap controls and preserve selectable full evidence IDs", () => {
  const operationsCss = readFileSync(resolve(process.cwd(), "src/renderer/operations/styles/operations.css"), "utf8");
  const host = mount(openScenario(scenarioById("needs-input")), { expanded: true });
  const evidence = host.querySelector("[data-evidence-id]") as HTMLElement;
  assert.ok(evidence.dataset.evidenceId);
  assert.equal(evidence.title, evidence.dataset.evidenceId);
  assert.equal(evidence.textContent, evidence.dataset.evidenceId);
  assert.match(operationsCss, /\.op-header[\s\S]*flex-wrap:\s*wrap/);
  assert.match(operationsCss, /\.op-actions[\s\S]*flex-wrap:\s*wrap/);
  assert.match(operationsCss, /\.op-row[\s\S]*flex-wrap:\s*wrap/);
  assert.match(operationsCss, /max-width:\s*560px[\s\S]*width:\s*100%/);
  assert.match(operationsCss, /\[data-evidence-id\][\s\S]*overflow-wrap:\s*anywhere/);
  assert.match(operationsCss, /\[data-evidence-id\][\s\S]*user-select:\s*text/);
});

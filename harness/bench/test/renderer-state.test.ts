import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/**
 * The renderer's state has four levels, and each owns what dies with it:
 *
 *   APP        the window: connection, team, palette, settings, model catalogue
 *   WORKSPACE  keyed by workspace id, outlives every session on it
 *   SESSION    keyed by session id, outlives every tab viewing it
 *   TAB        keyed by tab id, a VIEW of one session: open file, dialog, draft
 *
 * Closing a tab drops only TAB state; archiving a session drops SESSION state; WORKSPACE state
 * outlives sessions; APP outlives all. These are source-level assertions because the levels are
 * ownership rules, and the bug they prevent — one level's state outliving the thing it belonged
 * to — is invisible to a unit test of any single component.
 */
const app = () => fs.readFileSync(path.resolve("src/renderer/App.tsx"), "utf8");

test("TAB: an open file belongs to its tab, and goes when the tab does", () => {
  const s = app();
  // Not one global signal: as one, a file opened in tab A drew over tab B once A was closed
  // (owner, on the fleet).
  assert.ok(!/const \[file, setFile\] = createSignal/.test(s), "the open file is not a window-wide signal");
  assert.match(s, /const \[files, setFiles\] = createStore<Record<string, OpenFile \| undefined>>/, "it is keyed by tab");
  assert.match(s, /const file = \(\) => files\[selected\(\)\]/, "read for the selected tab");
  // Closing the tab drops it, so nothing of a closed tab is drawn over the next one.
  const close = s.slice(s.indexOf("const closeThread = (id: string) => {"), s.indexOf("/** A btw is removed"));
  assert.match(close, /setFiles\(id, undefined\)/, "a closed tab takes its open file with it");
});

test("TAB: a dialog is a view's state, not the window's", () => {
  const live = fs.readFileSync(path.resolve("src/renderer/live.ts"), "utf8");
  // A window-wide signal opened the `/model` picker in every tab at once.
  assert.ok(!/const \[dialog, setDialog\] = createSignal/.test(live), "the dialog is not a window-wide signal");
  assert.match(live, /const \[dialogs, setDialogs\] = createStore<Record<string, "model" \| undefined>>/, "it is keyed by tab");
  assert.match(live, /export const closeTab = \(tab: string\) =>/, "and TAB state is dropped as one thing");
  assert.match(app(), /live\.closeTab\(id\)/, "which closing a tab calls");
});

test("TAB state is dropped by closing a tab, and only that", () => {
  const s = app();
  const close = s.slice(s.indexOf("const closeThread = (id: string) => {"), s.indexOf("/** A btw is removed"));
  // A tab is a VIEW of a session: closing one must not touch the session's own state.
  for (const owned of ["live.thread(", "setSessions(", "discard("]) {
    assert.ok(!close.includes(owned), `closing a tab must not touch SESSION state (${owned})`);
  }
});

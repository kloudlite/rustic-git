import { test } from "node:test";
import assert from "node:assert/strict";
import { KEYS, inTerminal, mayAct } from "../../src/renderer/keys.ts";

/**
 * A terminal owns the keyboard. The owner typed `c` in a shell tab and zsh went into
 * `bck-i-search` (2026-09-17): an app handler on `document` acted on a key xterm was also
 * delivering. The rule is one line, so it is tested as one.
 */
const key = (init: Partial<KeyboardEvent> & { key: string }) => init as KeyboardEvent;

test("while a terminal has focus, only a chord it cannot mean reaches the app", () => {
  // Everything a person types at a shell.
  for (const k of ["c", "u", "r", "l", "Enter", "Escape", "ArrowUp", "ArrowDown", "Tab", " ", "Backspace"]) {
    assert.equal(mayAct(key({ key: k }), true), false, k);
  }
  // Shift alone is still typing: a capital C is a C.
  assert.equal(mayAct(key({ key: "C", shiftKey: true }), true), false);
  // The chords the app may keep.
  assert.equal(mayAct(key({ key: "l", metaKey: true }), true), true, "⌘L");
  assert.equal(mayAct(key({ key: "p", ctrlKey: true }), true), true, "^P");
  assert.equal(mayAct(key({ key: "Tab", shiftKey: true }), true), true, "shift+tab cycles the mode");
  // Outside a terminal nothing changes.
  for (const k of ["c", "Enter", "Escape"]) assert.equal(mayAct(key({ key: k }), false), true, k);
});

test("the shortcuts that would have eaten a keystroke are the ones now gated", () => {
  // These match with no modifier at all, which is exactly why the gate exists.
  assert.equal(KEYS.back.match(key({ key: "Escape" })), true);
  assert.equal(KEYS.mode.match(key({ key: "Tab", shiftKey: true })), true);
  // And the gate lets the second through in a terminal, but not the first.
  assert.equal(mayAct(key({ key: "Escape" }), true), false);
  assert.equal(mayAct(key({ key: "Tab", shiftKey: true }), true), true);
});

test("what counts as being in a terminal is the xterm element, not a guess", () => {
  const el = (cls: string) => ({ closest: (sel: string) => (sel === ".xterm" && cls === "xterm" ? {} : null) }) as unknown as Element;
  assert.equal(inTerminal(null), false);
  assert.equal(inTerminal(el("xterm")), true);
  assert.equal(inTerminal(el("composer")), false);
});

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Idle } from "../src/idle.ts";

test("idle only while no client is connected and nothing runs; any client or work clears it", () => {
  let busy = false;
  const i = new Idle(() => busy);
  assert.equal(typeof i.state().idleSince, "number");
  const since = i.state().idleSince;
  assert.equal(i.state().idleSince, since, "a second look keeps the first moment");
  i.opened();
  assert.deepEqual(i.state(), { clients: 1, busy: false, idleSince: null });
  i.closed();
  busy = true;
  assert.equal(i.state().idleSince, null);
  busy = false;
  assert.equal(typeof i.state().idleSince, "number");
});

test("work finishing with no client connected starts the idle clock", () => {
  let busy = true;
  const i = new Idle(() => busy);
  i.check();
  assert.equal(i.state().idleSince, null);
  busy = false;
  i.check();
  assert.equal(typeof i.state().idleSince, "number");
});

test("the signal waits out a continuous idle period: a client before it resets the clock", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-idle-"));
  const mark = path.join(dir, ".idle");
  let t = 0; // a fake clock in ms, so 300 s passes without waiting 300 s
  try {
    const i = new Idle(() => false, dir, 300_000, () => t);
    assert.equal(i.state().idle, undefined, "idle at t=0, but nothing is signalled yet");

    // A desktop tunnel reconnect at t=100 s: the clock starts over, and nothing was ever written.
    t = 100_000;
    i.opened();
    i.closed();
    assert.equal(fs.existsSync(mark), false, "a client inside the period leaves no signal behind");

    t = 399_000;
    i.check();
    assert.equal(fs.existsSync(mark), false, "one second short is not idle");
    t = 400_000;
    i.check();
    assert.equal(fs.readFileSync(mark, "utf8"), new Date(100_000).toISOString(), "and the moment is when idleness began");
    assert.equal(i.state().idle, new Date(100_000).toISOString());

    i.opened();
    assert.equal(fs.existsSync(mark), false, "a client clears the signal");
    assert.equal(i.state().idle, undefined);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("with a folder the moment is written to .idle and cleared by a client, so --ping can read it", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-idle-"));
  const mark = path.join(dir, ".idle");
  try {
    let busy = true;
    const i = new Idle(() => busy, dir);
    assert.equal(fs.existsSync(mark), false, "work running is not idle");
    busy = false;
    i.check();
    assert.match(fs.readFileSync(mark, "utf8"), /^\d{4}-\d\d-\d\dT.*Z$/);
    assert.equal(fs.readFileSync(mark, "utf8"), i.state().idle);
    const first = fs.readFileSync(mark, "utf8");
    i.check();
    assert.equal(fs.readFileSync(mark, "utf8"), first, "a second look keeps the first moment on disk too");
    i.opened();
    assert.equal(fs.existsSync(mark), false, "a client connecting clears it");
    assert.equal(i.state().idle, undefined);
    i.closed();
    assert.equal(fs.existsSync(mark), true, "and the last one leaving writes it again");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

import { test } from "node:test";
import assert from "node:assert/strict";
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

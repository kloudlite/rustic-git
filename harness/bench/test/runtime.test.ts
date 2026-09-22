import { test } from "node:test";
import assert from "node:assert/strict";
import { makeTurn } from "../src/runtime.ts";

test("makeTurn refuses to start without the two credentials", () => {
  assert.throws(() => makeTurn({} as NodeJS.ProcessEnv), /TYPESAFE_API_KEY/);
  assert.throws(() => makeTurn({ TYPESAFE_API_KEY: "x" } as NodeJS.ProcessEnv), /JEVHARN_API_KEY/);
});

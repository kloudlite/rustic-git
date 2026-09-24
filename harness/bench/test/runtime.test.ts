import { test } from "node:test";
import assert from "node:assert/strict";
import { makeTurn } from "../src/runtime.ts";

test("makeTurn refuses to start without TYPESAFE_API_KEY", () => {
  assert.throws(() => makeTurn({} as NodeJS.ProcessEnv), /TYPESAFE_API_KEY/);
});

test("a turn on a model whose provider key is missing fails with the reason", async () => {
  const turn = makeTurn({ TYPESAFE_API_KEY: "x" } as NodeJS.ProcessEnv);
  await assert.rejects(turn({ model: "deepseek/deepseek-v4-flash" } as Parameters<typeof turn>[0]), /DEEPSEEK_API_KEY/);
});

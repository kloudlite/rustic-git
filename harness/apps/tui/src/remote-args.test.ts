import { expect, test } from "bun:test";
import { pipeArgv } from "./remote-args.ts";

test("--ssh prefixes ssh", () => {
  expect(pipeArgv(["bun", "kl-tui", "--ssh", "-p", "22", "kl-host"])).toEqual(["ssh", "-p", "22", "kl-host"]);
});
test("--pipe runs the argv as given", () => {
  expect(pipeArgv(["bun", "kl-tui", "--pipe", "/bin/kl-connect", "bench-proxy", "--tui"])).toEqual(["/bin/kl-connect", "bench-proxy", "--tui"]);
});
test("no transport or an empty one is a usage error", () => {
  expect(pipeArgv(["bun", "kl-tui"])).toBeNull();
  expect(pipeArgv(["bun", "kl-tui", "--pipe"])).toBeNull();
});
